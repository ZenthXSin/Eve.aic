//! Real loopback HTTP, with a replaceable service and synthetic credentials only.
use eve_control_api::{ControlPhase, GenerationKey};
use eve_session_api::SessionKey;
use eve_web_panel::{LocalPanel, PanelConfig};
use eve_web_panel_api::*;
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TOKEN: &str = "synthetic-panel-token-never-persisted-123456789";

fn target() -> GenerationKey {
    GenerationKey {
        session: SessionKey::new("会话-A", "用户-A").unwrap(),
        task_id: "task-A".into(),
        controller_epoch: [7; 16],
        generation: 3,
    }
}

#[derive(Default)]
struct Service {
    calls: AtomicUsize,
    cancels: Mutex<Vec<GenerationKey>>,
    failure: Mutex<Option<PanelError>>,
}
impl Service {
    fn called(&self) -> PanelResult<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match *self.failure.lock().unwrap() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}
impl PanelService for Service {
    fn status(&self) -> PanelResult<PanelStatus> {
        self.called()?;
        Ok(PanelStatus {
            started_at_unix_ms: 100,
            qq: ChannelStatus {
                ready: true,
                closed: false,
                terminal_error: false,
                received: 2,
                completed: 1,
                sent: 1,
                failed: 0,
            },
        })
    }
    fn sessions(&self, after: Option<&str>, limit: usize) -> PanelResult<Page<SessionSummary>> {
        self.called()?;
        assert!((1..=100).contains(&limit));
        Ok(Page {
            items: if after.is_none() {
                vec![SessionSummary {
                    key: target().session,
                    revision: 5,
                    turn_count: 2,
                }]
            } else {
                vec![]
            },
            next_cursor: Some("会话-A".into()),
        })
    }
    fn tasks(&self, _: Option<&str>, limit: usize) -> PanelResult<Page<TaskSummary>> {
        self.called()?;
        assert!((1..=100).contains(&limit));
        Ok(Page {
            items: vec![TaskSummary {
                key: target(),
                phase: ControlPhase::Generating,
                cancel_requested: false,
                events_retired: false,
                turn_id: Some(2),
                commit: None,
                started_tools: Some(0),
            }],
            next_cursor: None,
        })
    }
    fn session(
        &self,
        key: &SessionKey,
        before: Option<u64>,
        limit: usize,
    ) -> PanelResult<SessionDetail> {
        self.called()?;
        assert!((1..=50).contains(&limit));
        if key != &target().session {
            return Err(PanelError::NotFound);
        }
        Ok(SessionDetail {
            session: SessionHistory {
                key: key.clone(),
                revision: 5,
                turns: vec![PanelTurn {
                    id: before.unwrap_or(3) - 1,
                    input: "正文 <script>".into(),
                    input_truncated: false,
                    status: PanelTurnStatus::Pending,
                }],
                next_before: Some(1),
            },
            control: None,
        })
    }
    fn cancel(&self, value: &GenerationKey) -> PanelResult<CancelStatus> {
        self.called()?;
        if value != &target() {
            return Err(PanelError::Stale);
        }
        self.cancels.lock().unwrap().push(value.clone());
        Ok(CancelStatus::Requested)
    }
}

async fn panel(service: Arc<Service>) -> LocalPanel {
    LocalPanel::bind(
        PanelConfig {
            address: "127.0.0.1:0".parse().unwrap(),
            token: TOKEN.into(),
        },
        service,
    )
    .await
    .unwrap()
}
fn url(panel: &LocalPanel, path: &str) -> String {
    format!("http://{}{path}", panel.address())
}
fn client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .unwrap()
}
async fn assert_code(response: reqwest::Response, status: StatusCode, code: &str) {
    assert_eq!(response.status(), status);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!({"error":code})
    );
}

#[tokio::test]
async fn static_assets_have_security_headers_and_never_contain_the_token() {
    let service = Arc::new(Service::default());
    let panel = panel(service.clone()).await;
    for path in ["/", "/app.js", "/style.css"] {
        let response = client().get(url(&panel, path)).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(response.headers()["x-frame-options"], "DENY");
        assert_eq!(response.headers()["referrer-policy"], "no-referrer");
        assert!(
            response.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("frame-ancestors 'none'")
        );
        let body = response.text().await.unwrap();
        assert!(!body.contains(TOKEN));
        assert!(!body.contains("EVE_OPENAI_API_KEY"));
        assert!(!body.contains("EVE_JEV_API_KEY"));
    }
    assert_eq!(service.calls.load(Ordering::SeqCst), 0);
    panel.stop().await.unwrap();
}

#[tokio::test]
async fn authentication_host_origin_and_form_posts_fail_before_the_service() {
    let service = Arc::new(Service::default());
    let panel = panel(service.clone()).await;
    let client = client();
    assert_code(
        client.get(url(&panel, "/api/status")).send().await.unwrap(),
        StatusCode::UNAUTHORIZED,
        "unauthorized",
    )
    .await;
    assert_code(
        client
            .get(url(&panel, "/api/status"))
            .bearer_auth(format!("{TOKEN}x"))
            .send()
            .await
            .unwrap(),
        StatusCode::UNAUTHORIZED,
        "unauthorized",
    )
    .await;
    assert_code(
        client
            .get(url(&panel, "/api/status"))
            .bearer_auth(TOKEN)
            .header("Host", "evil.test")
            .send()
            .await
            .unwrap(),
        StatusCode::FORBIDDEN,
        "invalid_host",
    )
    .await;
    for request in [
        client
            .get(url(&panel, "/api/status"))
            .bearer_auth(TOKEN)
            .header("Origin", "https://evil.test"),
        client
            .get(url(&panel, "/api/status"))
            .bearer_auth(TOKEN)
            .header("Sec-Fetch-Site", "cross-site"),
    ] {
        assert_code(
            request.send().await.unwrap(),
            StatusCode::FORBIDDEN,
            "invalid_origin",
        )
        .await;
    }
    assert_code(
        client
            .post(url(&panel, "/api/cancel"))
            .bearer_auth(TOKEN)
            .header("Content-Type", "text/plain")
            .body(json!({"target":target()}).to_string())
            .send()
            .await
            .unwrap(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "json_required",
    )
    .await;
    assert_code(
        client
            .get(url(&panel, &format!("/api/status?token={TOKEN}")))
            .send()
            .await
            .unwrap(),
        StatusCode::UNAUTHORIZED,
        "unauthorized",
    )
    .await;
    assert_eq!(service.calls.load(Ordering::SeqCst), 0);
    let good = client
        .get(url(&panel, "/api/status"))
        .bearer_auth(TOKEN)
        .header("Origin", format!("http://{}", panel.address()))
        .send()
        .await
        .unwrap();
    assert_eq!(good.status(), StatusCode::OK);
    assert_eq!(good.json::<Value>().await.unwrap()["qq"]["ready"], true);
    panel.stop().await.unwrap();
}

#[tokio::test]
async fn strict_json_queries_methods_and_body_limit_never_admit_invalid_actions() {
    let service = Arc::new(Service::default());
    let panel = panel(service.clone()).await;
    let client = client();
    for query in [
        "limit=0",
        "limit=101",
        "limit=-1",
        "limit=bad",
        "limit=1&limit=2",
        "unknown=1",
        "after=",
        "after=%0A",
    ] {
        assert_code(
            client
                .get(url(&panel, &format!("/api/tasks?{query}")))
                .bearer_auth(TOKEN)
                .send()
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
            "invalid_query",
        )
        .await;
    }
    for body in [json!({"target":target(), "extra":1}).to_string(), "{}".into(), "{".into(),
        format!("{{\"target\":{},\"target\":{}}}", json!(target()), json!(target())),
        json!({"target":{"session":target().session,"task_id":"task-A","controller_epoch":[7],"generation":3}}).to_string()] {
        assert_code(client.post(url(&panel, "/api/cancel")).bearer_auth(TOKEN).header("Content-Type", "application/json").body(body).send().await.unwrap(), StatusCode::BAD_REQUEST, "invalid_json").await;
    }
    for field in ["generation", "task", "session"] {
        let mut bad = target();
        match field {
            "generation" => bad.generation = 0,
            "task" => bad.task_id = " bad ".into(),
            _ => bad.session.user_id.clear(),
        }
        assert_code(
            client
                .post(url(&panel, "/api/cancel"))
                .bearer_auth(TOKEN)
                .json(&json!({"target":bad}))
                .send()
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
            "invalid_input",
        )
        .await;
    }
    for method in [Method::GET, Method::PUT, Method::DELETE, Method::OPTIONS] {
        let response = client
            .request(method, url(&panel, "/api/cancel"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
    let response = client
        .post(url(&panel, "/api/session"))
        .bearer_auth(TOKEN)
        .json(&json!({"key":target().session,"limit":51}))
        .send()
        .await
        .unwrap();
    assert_code(response, StatusCode::BAD_REQUEST, "invalid_input").await;
    let response = client
        .post(url(&panel, "/api/session"))
        .bearer_auth(TOKEN)
        .header("Content-Type", "application/json")
        .body("x".repeat(16385))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!response.text().await.unwrap().contains(TOKEN));
    assert_eq!(service.calls.load(Ordering::SeqCst), 0);
    panel.stop().await.unwrap();
}

#[tokio::test]
async fn exact_generations_pagination_and_fixed_errors_preserve_service_contract() {
    let service = Arc::new(Service::default());
    let panel = panel(service.clone()).await;
    let client = client();
    let sessions = client
        .get(url(&panel, "/api/sessions?limit=1"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(sessions["sessions"][0]["key"], json!(target().session));
    assert_eq!(sessions["next_cursor"], "会话-A");
    let history = client
        .post(url(&panel, "/api/session"))
        .bearer_auth(TOKEN)
        .json(&json!({"key":target().session,"before":3,"limit":1}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(history["session"]["turns"][0]["id"], 2);
    assert_eq!(history["session"]["next_before"], 1);
    for field in ["generation", "epoch", "owner", "task"] {
        let mut stale = target();
        match field {
            "generation" => stale.generation += 1,
            "epoch" => stale.controller_epoch[0] += 1,
            "owner" => stale.session.user_id = "other".into(),
            _ => stale.task_id = "other".into(),
        }
        assert_code(
            client
                .post(url(&panel, "/api/cancel"))
                .bearer_auth(TOKEN)
                .json(&json!({"target":stale}))
                .send()
                .await
                .unwrap(),
            StatusCode::CONFLICT,
            "stale_generation",
        )
        .await;
    }
    let response = client
        .post(url(&panel, "/api/cancel"))
        .bearer_auth(TOKEN)
        .json(&json!({"target":target()}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!({"status":"requested"})
    );
    assert_eq!(*service.cancels.lock().unwrap(), vec![target()]);
    *service.failure.lock().unwrap() = Some(PanelError::Unavailable);
    assert_code(
        client
            .get(url(&panel, "/api/status"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap(),
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
    )
    .await;
    panel.stop().await.unwrap();
}

#[tokio::test]
async fn duplicate_headers_and_slow_body_capacity_are_bounded_then_shutdown_releases_port() {
    let service = Arc::new(Service::default());
    let panel = panel(service.clone()).await;
    for extra in [
        format!("Authorization: Bearer {TOKEN}\r\n"),
        "Origin: http://evil.test\r\nOrigin: http://evil.test\r\n".into(),
    ] {
        let mut stream = TcpStream::connect(panel.address()).await.unwrap();
        stream.write_all(format!("GET /api/status HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {TOKEN}\r\n{extra}Connection: close\r\n\r\n", panel.address()).as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 401") || response.starts_with("HTTP/1.1 403"));
    }
    let mut streams = Vec::new();
    for _ in 0..16 {
        let mut stream = TcpStream::connect(panel.address()).await.unwrap();
        stream.write_all(format!("POST /api/session HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: 4\r\nExpect: 100-continue\r\n\r\n", panel.address()).as_bytes()).await.unwrap();
        let mut interim = [0; 25];
        stream.read_exact(&mut interim).await.unwrap();
        assert_eq!(&interim, b"HTTP/1.1 100 Continue\r\n\r\n");
        streams.push(stream);
    }
    assert_code(
        client()
            .get(url(&panel, "/api/status"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap(),
        StatusCode::TOO_MANY_REQUESTS,
        "busy",
    )
    .await;
    assert_eq!(service.calls.load(Ordering::SeqCst), 0);
    drop(streams);
    let address = panel.address();
    panel.stop().await.unwrap();
    let restarted = LocalPanel::bind(
        PanelConfig {
            address,
            token: format!("{TOKEN}-new"),
        },
        service,
    )
    .await
    .unwrap();
    assert_code(
        client()
            .get(url(&restarted, "/api/status"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap(),
        StatusCode::UNAUTHORIZED,
        "unauthorized",
    )
    .await;
    restarted.stop().await.unwrap();
}

#[tokio::test]
async fn configuration_rejects_nonloopback_and_bad_credentials_without_binding() {
    for (address, token) in [
        ("0.0.0.0:0", TOKEN),
        ("127.0.0.1:0", "short"),
        ("127.0.0.1:0", "contains space synthetic credential 12345"),
    ] {
        let result = LocalPanel::bind(
            PanelConfig {
                address: address.parse().unwrap(),
                token: token.into(),
            },
            Arc::new(Service::default()),
        )
        .await;
        assert!(result.is_err());
        assert!(!result.err().unwrap().to_string().contains(token));
    }
}

#[tokio::test]
async fn stopping_after_authentication_but_before_body_completion_never_admits_cancel() {
    let service = Arc::new(Service::default());
    let panel = panel(service.clone()).await;
    let body = json!({"target":target()}).to_string();
    let mut stream = TcpStream::connect(panel.address()).await.unwrap();
    stream.write_all(format!("POST /api/cancel HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n", panel.address(), body.len()).as_bytes()).await.unwrap();
    let mut interim = [0; 25];
    stream.read_exact(&mut interim).await.unwrap();
    assert_eq!(&interim, b"HTTP/1.1 100 Continue\r\n\r\n");
    stream.write_all(&body.as_bytes()[..1]).await.unwrap();
    panel.request_stop();
    stream.write_all(&body.as_bytes()[1..]).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 503"));
    assert!(response.contains("stopping"));
    assert_eq!(service.calls.load(Ordering::SeqCst), 0);
    assert!(service.cancels.lock().unwrap().is_empty());
    panel.stop().await.unwrap();
}
