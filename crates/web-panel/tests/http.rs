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

#[tokio::test]
async fn extension_endpoints_require_authentication_and_validate_before_optional_services() {
    let service = Arc::new(Service::default());
    let panel = panel(service.clone()).await;
    for path in [
        "/api/plugins",
        "/api/plugins/operations",
        "/api/plugin-pages",
    ] {
        assert_code(
            client().get(url(&panel, path)).send().await.unwrap(),
            StatusCode::UNAUTHORIZED,
            "unauthorized",
        )
        .await;
        assert_code(
            client()
                .get(url(&panel, path))
                .bearer_auth(TOKEN)
                .send()
                .await
                .unwrap(),
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
        )
        .await;
    }
    for (path, invalid) in [
        (
            "/api/plugins/action",
            json!({"instance":"i","plugin_id":"../bad","expected_state":"Active","action":"stop"}),
        ),
        ("/api/plugins/ack", json!({"id":0})),
        (
            "/api/plugin-pages/read",
            json!({"plugin_id":"eve.config","page_id":"../bad"}),
        ),
        (
            "/api/plugin-pages/save",
            json!({"plugin_id":"eve.config","page_id":"runtime.llm","instance":"i","expected_revision":0,"values":{}}),
        ),
    ] {
        assert_code(
            client()
                .post(url(&panel, path))
                .bearer_auth(TOKEN)
                .json(&invalid)
                .send()
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
            "invalid_input",
        )
        .await;
    }
    assert_eq!(service.calls.load(Ordering::SeqCst), 0);
    panel.stop().await.unwrap();
}

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

struct Judgments {
    calls: Mutex<Vec<(Option<u64>, usize)>>,
}
impl PanelService for Judgments {
    fn status(&self) -> PanelResult<PanelStatus> {
        Err(PanelError::Unavailable)
    }
    fn sessions(&self, _: Option<&str>, _: usize) -> PanelResult<Page<SessionSummary>> {
        Err(PanelError::Unavailable)
    }
    fn tasks(&self, _: Option<&str>, _: usize) -> PanelResult<Page<TaskSummary>> {
        Err(PanelError::Unavailable)
    }
    fn session(&self, _: &SessionKey, _: Option<u64>, _: usize) -> PanelResult<SessionDetail> {
        Err(PanelError::Unavailable)
    }
    fn cancel(&self, _: &GenerationKey) -> PanelResult<CancelStatus> {
        Err(PanelError::Unavailable)
    }
    fn judgments(&self, before: Option<u64>, limit: usize) -> PanelResult<JudgmentLog> {
        self.calls.lock().unwrap().push((before, limit));
        Ok(JudgmentLog {
            mode: "primary",
            capacity: 128,
            recorded_total: 2,
            evicted: 0,
            items: vec![JudgmentView {
                sequence: 2,
                finished_at_unix_ms: 1_700_000_000_000,
                elapsed_micros: 900,
                result: "decided",
                failure: None,
                intents: vec!["cancel"],
                coverage: "complete",
                counts: Some(JudgmentCounts {
                    rules: 1,
                    auxiliary: 0,
                    primary: 0,
                    classifier_calls: 0,
                    model_provider_calls: 0,
                    fallbacks: 0,
                }),
                steps: vec![JudgmentStep {
                    kind: "stage",
                    name: "rules",
                    outcome: Some("completed"),
                    elapsed_micros: Some(12),
                }],
                fallbacks: vec![],
            }],
            next_before: Some(2),
        })
    }
}

#[tokio::test]
async fn judgment_log_is_authenticated_bounded_and_unavailable_by_default() {
    let client = client();
    // 旧实现未覆盖新方法时，默认是明确的不可用，而不是空列表。
    let legacy = Arc::new(Service::default());
    let panel_legacy = panel(legacy.clone()).await;
    assert_code(
        client
            .get(url(&panel_legacy, "/api/judgments"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap(),
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
    )
    .await;
    panel_legacy.stop().await.unwrap();

    let service = Arc::new(Judgments {
        calls: Mutex::new(vec![]),
    });
    let panel = LocalPanel::bind(
        PanelConfig {
            address: "127.0.0.1:0".parse().unwrap(),
            token: TOKEN.into(),
        },
        service.clone(),
    )
    .await
    .unwrap();
    assert_code(
        client
            .get(url(&panel, "/api/judgments"))
            .send()
            .await
            .unwrap(),
        StatusCode::UNAUTHORIZED,
        "unauthorized",
    )
    .await;
    for query in [
        "limit=0",
        "limit=51",
        "limit=-1",
        "before=0",
        "before=-1",
        "before=bad",
        "before=1&before=2",
        "after=1",
    ] {
        assert_code(
            client
                .get(url(&panel, &format!("/api/judgments?{query}")))
                .bearer_auth(TOKEN)
                .send()
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
            "invalid_query",
        )
        .await;
    }
    assert!(service.calls.lock().unwrap().is_empty());
    let response = client
        .get(url(&panel, "/api/judgments"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let body = response.json::<Value>().await.unwrap();
    assert_eq!(body["mode"], "primary");
    assert_eq!(body["items"][0]["intents"], json!(["cancel"]));
    assert_eq!(body["items"][0]["failure"], Value::Null);
    assert_eq!(body["items"][0]["counts"]["rules"], 1);
    assert_eq!(body["items"][0]["steps"][0]["name"], "rules");
    assert_eq!(body["next_before"], 2);
    client
        .get(url(&panel, "/api/judgments?before=2&limit=50"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(
        *service.calls.lock().unwrap(),
        vec![(None, 25), (Some(2), 50)]
    );
    panel.stop().await.unwrap();
}

struct Goals {
    calls: Mutex<Vec<String>>,
}
impl PanelService for Goals {
    fn status(&self) -> PanelResult<PanelStatus> {
        Err(PanelError::Unavailable)
    }
    fn sessions(&self, _: Option<&str>, _: usize) -> PanelResult<Page<SessionSummary>> {
        Err(PanelError::Unavailable)
    }
    fn tasks(&self, _: Option<&str>, _: usize) -> PanelResult<Page<TaskSummary>> {
        Err(PanelError::Unavailable)
    }
    fn session(&self, _: &SessionKey, _: Option<u64>, _: usize) -> PanelResult<SessionDetail> {
        Err(PanelError::Unavailable)
    }
    fn cancel(&self, _: &GenerationKey) -> PanelResult<CancelStatus> {
        Err(PanelError::Unavailable)
    }
    fn goals(&self, after: Option<&str>, limit: usize) -> PanelResult<GoalPage> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("list:{after:?}:{limit}"));
        Ok(GoalPage {
            subject_id: "eve".into(),
            revision: 7,
            items: vec![summary()],
            next_cursor: Some("目标-1".into()),
        })
    }
    fn goal(&self, id: &str) -> PanelResult<GoalDetail> {
        self.calls.lock().unwrap().push(format!("detail:{id}"));
        if id != "目标-1" {
            return Err(PanelError::NotFound);
        }
        Ok(GoalDetail {
            subject_id: "eve".into(),
            revision: 7,
            goal: summary(),
            verification: "user-goal:v1".into(),
            stop_condition: "user-confirmation".into(),
            wait_reason: Some("等待反思草稿".into()),
            block_reason: None,
            expires_at_ms: None,
            budget: GoalBudget {
                max_model_requests: 1,
                max_tool_calls: 0,
                max_attempts: 1,
                timeout_ms: 30_000,
            },
            execution: None,
            feedback: None,
            events: vec![],
            events_omitted: 0,
            reflections: vec![ReflectionView {
                goal_id: "反思-2".into(),
                parent_revision: Some(2),
                status: "completed",
                current: true,
                draft_state: "saved",
                draft: Some(ReflectionDraft {
                    summary: "<b>草稿</b>".into(),
                    next_step: "请确认".into(),
                    needs_user_input: true,
                }),
            }],
            reflection_check: "ok",
        })
    }
}
fn summary() -> GoalSummary {
    GoalSummary {
        id: "目标-1".into(),
        revision: 2,
        status: "waiting",
        priority: 50,
        source_kind: "user",
        source_channel: "qq.goal".into(),
        visibility: "user",
        owner: Some("用户-A".into()),
        reflection_of: None,
        reflections: 1,
        description: "整理房间".into(),
        description_truncated: false,
    }
}

#[tokio::test]
async fn goal_views_are_authenticated_strictly_queried_and_unavailable_by_default() {
    let client = client();
    let legacy = Arc::new(Service::default());
    let panel_legacy = panel(legacy.clone()).await;
    for path in ["/api/goals", "/api/goal?id=目标-1"] {
        assert_code(
            client
                .get(url(&panel_legacy, path))
                .bearer_auth(TOKEN)
                .send()
                .await
                .unwrap(),
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
        )
        .await;
    }
    panel_legacy.stop().await.unwrap();

    let service = Arc::new(Goals {
        calls: Mutex::new(vec![]),
    });
    let panel = LocalPanel::bind(
        PanelConfig {
            address: "127.0.0.1:0".parse().unwrap(),
            token: TOKEN.into(),
        },
        service.clone(),
    )
    .await
    .unwrap();
    for path in ["/api/goals", "/api/goal?id=目标-1"] {
        assert_code(
            client.get(url(&panel, path)).send().await.unwrap(),
            StatusCode::UNAUTHORIZED,
            "unauthorized",
        )
        .await;
        for method in [Method::PUT, Method::DELETE] {
            let response = client
                .request(method, url(&panel, path))
                .bearer_auth(TOKEN)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        }
    }
    let long = "a".repeat(257);
    for path in [
        "/api/goals?limit=0".to_owned(),
        "/api/goals?limit=101".into(),
        "/api/goals?after=".into(),
        format!("/api/goals?after={long}"),
        "/api/goals?after=%01".into(),
        "/api/goals?before=1".into(),
        "/api/goal".into(),
        "/api/goal?id=".into(),
        "/api/goal?id=%20padded".into(),
        "/api/goal?id=bad%0A".into(),
        format!("/api/goal?id={long}"),
        "/api/goal?id=a&id=b".into(),
        "/api/goal?id=a&limit=1".into(),
    ] {
        assert_code(
            client
                .get(url(&panel, &path))
                .bearer_auth(TOKEN)
                .send()
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
            "invalid_query",
        )
        .await;
    }
    assert!(service.calls.lock().unwrap().is_empty());

    let response = client
        .get(url(&panel, "/api/goals?after=目标-0&limit=100"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let body = response.json::<Value>().await.unwrap();
    assert_eq!(body["revision"], 7);
    assert_eq!(body["items"][0]["status"], "waiting");
    assert_eq!(body["items"][0]["owner"], "用户-A");
    assert_eq!(body["next_cursor"], "目标-1");

    let body = client
        .get(url(&panel, "/api/goal?id=目标-1"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(body["goal"]["id"], "目标-1");
    assert_eq!(body["reflections"][0]["current"], true);
    assert_eq!(body["reflections"][0]["draft"]["summary"], "<b>草稿</b>");
    assert_eq!(body["reflection_check"], "ok");
    assert_code(
        client
            .get(url(&panel, "/api/goal?id=missing"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap(),
        StatusCode::NOT_FOUND,
        "not_found",
    )
    .await;
    assert_eq!(
        *service.calls.lock().unwrap(),
        vec![
            "list:Some(\"目标-0\"):100".to_owned(),
            "detail:目标-1".into(),
            "detail:missing".into(),
        ]
    );
    panel.stop().await.unwrap();
}

struct Memories {
    calls: Mutex<Vec<String>>,
}
fn memory_scope(user: &str) -> eve_memory_api::MemoryScope {
    eve_memory_api::MemoryScope {
        channel: "qq".into(),
        session_id: "会话-A".into(),
        user_id: user.into(),
    }
}
impl PanelService for Memories {
    fn status(&self) -> PanelResult<PanelStatus> {
        Err(PanelError::Unavailable)
    }
    fn sessions(&self, _: Option<&str>, _: usize) -> PanelResult<Page<SessionSummary>> {
        Err(PanelError::Unavailable)
    }
    fn tasks(&self, _: Option<&str>, _: usize) -> PanelResult<Page<TaskSummary>> {
        Err(PanelError::Unavailable)
    }
    fn session(&self, _: &SessionKey, _: Option<u64>, _: usize) -> PanelResult<SessionDetail> {
        Err(PanelError::Unavailable)
    }
    fn cancel(&self, _: &GenerationKey) -> PanelResult<CancelStatus> {
        Err(PanelError::Unavailable)
    }
    fn memory_scopes(
        &self,
        after: Option<&eve_memory_api::MemoryScope>,
        limit: usize,
    ) -> PanelResult<MemoryScopePage> {
        self.calls.lock().unwrap().push(format!(
            "scopes:{:?}:{limit}",
            after.map(|scope| scope.user_id.clone())
        ));
        Ok(MemoryScopePage {
            items: vec![MemoryScopeSummary {
                scope: memory_scope("用户-A"),
                revision: 3,
                evidence: 2,
                confirmed: 1,
                revoked: 0,
            }],
            next_after: Some(memory_scope("用户-A")),
        })
    }
    fn memory(&self, scope: &eve_memory_api::MemoryScope) -> PanelResult<MemoryDetail> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("detail:{}", scope.user_id));
        if scope.user_id != "用户-A" {
            return Err(PanelError::NotFound);
        }
        Ok(MemoryDetail {
            scope: scope.clone(),
            revision: 3,
            evidence: 2,
            preferences: vec![PreferenceView {
                id: "偏好-1".into(),
                status: "confirmed",
                revision: 1,
                effective: true,
                text: "<b>先给结论</b>".into(),
                text_truncated: false,
                history: vec![PreferenceVersionView {
                    revision: 1,
                    status: "confirmed",
                    at_ms: 10,
                    current: true,
                    text: "<b>先给结论</b>".into(),
                    text_truncated: false,
                    evidence_id: "证据-1".into(),
                    evidence_kind: "user_statement",
                }],
            }],
        })
    }
    fn memory_learning(&self, scope: &eve_memory_api::MemoryScope) -> PanelResult<LearningView> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("learning:{}", scope.user_id));
        Ok(LearningView {
            scope: scope.clone(),
            autonomous: false,
            jobs: vec![LearningJobView {
                batch_id: "批次-1".into(),
                status: "failed",
                failure: Some("timeout"),
                started_at_ms: 1,
                finished_at_ms: Some(2),
                evidence: 1,
                candidates: 0,
            }],
            candidates: vec![LearningCandidateView {
                id: "候选-1".into(),
                batch_id: "批次-0".into(),
                text: "<i>候选</i>".into(),
                confidence: 70,
                evidence_ids: vec!["证据-1".into()],
                created_at_ms: 1,
                expires_at_ms: 2,
                expired: true,
                saved: None,
                decisions: vec![LearningDecisionView {
                    sequence: 1,
                    at_ms: 1,
                    action: "defer",
                    update_preference: None,
                    update_revision: None,
                    reason: "evidence_threshold",
                    policy_version: "v1".into(),
                    memory_revision: 1,
                }],
                decisions_total: 1,
            }],
            decisions_total: 1,
        })
    }
    fn memory_evidence(
        &self,
        scope: &eve_memory_api::MemoryScope,
        id: &str,
    ) -> PanelResult<MemoryEvidenceDetail> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("evidence:{}:{id}", scope.user_id));
        if id != "证据-1" {
            return Err(PanelError::NotFound);
        }
        Ok(MemoryEvidenceDetail {
            scope: scope.clone(),
            id: id.into(),
            revision: 1,
            at_ms: 10,
            kind: "completed_interaction",
            user_text: "请简短".into(),
            user_text_truncated: false,
            assistant_text: Some("好的".into()),
            assistant_text_truncated: false,
            turn_id: Some(2),
            references: vec![EvidenceReference {
                preference_id: "偏好-1".into(),
                revision: 1,
                current: true,
                effective: true,
            }],
            references_total: 1,
        })
    }
}

#[tokio::test]
async fn memory_views_are_authenticated_strict_json_and_unavailable_by_default() {
    let client = client();
    let scope = json!({"channel": "qq", "session_id": "会话-A", "user_id": "用户-A"});
    let paths = [
        ("/api/memory/scopes", json!({})),
        ("/api/memory/scope", json!({"scope": scope})),
        (
            "/api/memory/evidence",
            json!({"scope": scope, "id": "证据-1"}),
        ),
        ("/api/memory/learning", json!({"scope": scope})),
    ];
    let legacy = Arc::new(Service::default());
    let panel_legacy = panel(legacy.clone()).await;
    for (path, body) in &paths {
        assert_code(
            client
                .post(url(&panel_legacy, path))
                .bearer_auth(TOKEN)
                .json(body)
                .send()
                .await
                .unwrap(),
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
        )
        .await;
    }
    panel_legacy.stop().await.unwrap();

    let service = Arc::new(Memories {
        calls: Mutex::new(vec![]),
    });
    let panel = LocalPanel::bind(
        PanelConfig {
            address: "127.0.0.1:0".parse().unwrap(),
            token: TOKEN.into(),
        },
        service.clone(),
    )
    .await
    .unwrap();
    for (path, body) in &paths {
        assert_code(
            client
                .post(url(&panel, path))
                .json(body)
                .send()
                .await
                .unwrap(),
            StatusCode::UNAUTHORIZED,
            "unauthorized",
        )
        .await;
        let response = client
            .get(url(&panel, path))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
    let blank = json!({"channel": "qq", "session_id": "", "user_id": "用户-A"});
    let control = json!({"channel": "qq", "session_id": "会话\n", "user_id": "用户-A"});
    let extra = json!({"channel": "qq", "session_id": "会话-A", "user_id": "用户-A", "x": 1});
    for (path, body, code) in [
        ("/api/memory/scopes", json!({"limit": 0}), "invalid_input"),
        ("/api/memory/scopes", json!({"limit": 101}), "invalid_input"),
        (
            "/api/memory/scopes",
            json!({"after": blank}),
            "invalid_input",
        ),
        (
            "/api/memory/scopes",
            json!({"after": control}),
            "invalid_input",
        ),
        ("/api/memory/scopes", json!({"before": 1}), "invalid_json"),
        (
            "/api/memory/scopes",
            json!({"after": extra}),
            "invalid_json",
        ),
        ("/api/memory/scope", json!({}), "invalid_json"),
        (
            "/api/memory/scope",
            json!({"scope": blank}),
            "invalid_input",
        ),
        (
            "/api/memory/scope",
            json!({"scope": scope, "limit": 1}),
            "invalid_json",
        ),
        (
            "/api/memory/evidence",
            json!({"scope": scope}),
            "invalid_json",
        ),
        (
            "/api/memory/evidence",
            json!({"scope": scope, "id": ""}),
            "invalid_input",
        ),
        (
            "/api/memory/evidence",
            json!({"scope": scope, "id": " 证据"}),
            "invalid_input",
        ),
        (
            "/api/memory/evidence",
            json!({"scope": scope, "id": "a".repeat(257)}),
            "invalid_input",
        ),
        (
            "/api/memory/evidence",
            json!({"scope": control, "id": "证据-1"}),
            "invalid_input",
        ),
        (
            "/api/memory/learning",
            json!({"scope": blank}),
            "invalid_input",
        ),
        (
            "/api/memory/learning",
            json!({"scope": scope, "id": "x"}),
            "invalid_json",
        ),
    ] {
        assert_code(
            client
                .post(url(&panel, path))
                .bearer_auth(TOKEN)
                .json(&body)
                .send()
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
            code,
        )
        .await;
    }
    assert_code(
        client
            .post(url(&panel, "/api/memory/scope"))
            .bearer_auth(TOKEN)
            .body(json!({"scope": scope}).to_string())
            .send()
            .await
            .unwrap(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "json_required",
    )
    .await;
    assert!(service.calls.lock().unwrap().is_empty());

    let response = client
        .post(url(&panel, "/api/memory/scopes"))
        .bearer_auth(TOKEN)
        .json(&json!({"after": scope, "limit": 100}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let body = response.json::<Value>().await.unwrap();
    assert_eq!(body["items"][0]["scope"]["user_id"], "用户-A");
    assert_eq!(body["items"][0]["confirmed"], 1);
    assert_eq!(body["next_after"]["user_id"], "用户-A");
    let body = client
        .post(url(&panel, "/api/memory/scope"))
        .bearer_auth(TOKEN)
        .json(&json!({"scope": scope}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(body["preferences"][0]["effective"], true);
    assert_eq!(
        body["preferences"][0]["history"][0]["evidence_kind"],
        "user_statement"
    );
    let body = client
        .post(url(&panel, "/api/memory/evidence"))
        .bearer_auth(TOKEN)
        .json(&json!({"scope": scope, "id": "证据-1"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(body["assistant_text"], "好的");
    assert_eq!(body["references_total"], 1);
    let body = client
        .post(url(&panel, "/api/memory/learning"))
        .bearer_auth(TOKEN)
        .json(&json!({"scope": scope}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(body["jobs"][0]["failure"], "timeout");
    assert_eq!(body["candidates"][0]["saved"], Value::Null);
    assert_eq!(
        body["candidates"][0]["decisions"][0]["reason"],
        "evidence_threshold"
    );
    let other = json!({"channel": "qq", "session_id": "会话-A", "user_id": "用户-B"});
    for (path, body) in [
        ("/api/memory/scope", json!({"scope": other})),
        (
            "/api/memory/evidence",
            json!({"scope": scope, "id": "缺失"}),
        ),
    ] {
        assert_code(
            client
                .post(url(&panel, path))
                .bearer_auth(TOKEN)
                .json(&body)
                .send()
                .await
                .unwrap(),
            StatusCode::NOT_FOUND,
            "not_found",
        )
        .await;
    }
    assert_eq!(
        *service.calls.lock().unwrap(),
        vec![
            "scopes:Some(\"用户-A\"):100".to_owned(),
            "detail:用户-A".into(),
            "evidence:用户-A:证据-1".into(),
            "learning:用户-A".into(),
            "detail:用户-B".into(),
            "evidence:用户-A:缺失".into(),
        ]
    );
    panel.stop().await.unwrap();
}
