//! 仅绑定环回地址的 HTTP 管理面板；所有业务由公开 PanelService 实现。
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Query, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use eve_control_api::GenerationKey;
use eve_memory_api::MemoryScope;
use eve_session_api::SessionKey;
use eve_web_panel_api::{PanelError, PanelService};
use ring::{
    hmac,
    rand::{SecureRandom, SystemRandom},
};
use serde::Deserialize;
use serde_json::json;
use std::{
    fmt,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Semaphore, watch},
    task::JoinHandle,
};

/// 不实现 Debug：令牌仅在可信宿主启动时读取，不进入页面、URL、日志或持久配置。
pub struct PanelConfig {
    pub address: SocketAddr,
    pub token: String,
}
impl PanelConfig {
    pub fn validate(&self) -> Result<(), PanelFailure> {
        if !self.address.ip().is_loopback()
            || !(32..=256).contains(&self.token.len())
            || !self.token.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(PanelFailure);
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug)]
pub struct PanelFailure;
impl fmt::Display for PanelFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("本机面板配置、监听或收尾失败；请检查环回地址、端口和专用令牌")
    }
}
impl std::error::Error for PanelFailure {}

struct Shared {
    service: Arc<dyn PanelService>,
    authority: String,
    token_key: hmac::Key,
    token_tag: hmac::Tag,
    open: AtomicBool,
    slots: Semaphore,
}

pub struct LocalPanel {
    address: SocketAddr,
    shared: Arc<Shared>,
    stop: watch::Sender<bool>,
    finished: watch::Receiver<bool>,
    task: Option<JoinHandle<std::io::Result<()>>>,
}
impl LocalPanel {
    pub async fn bind(
        config: PanelConfig,
        service: Arc<dyn PanelService>,
    ) -> Result<Self, PanelFailure> {
        config.validate()?;
        let listener = TcpListener::bind(config.address)
            .await
            .map_err(|_| PanelFailure)?;
        let address = listener.local_addr().map_err(|_| PanelFailure)?;
        let mut random = [0u8; 32];
        SystemRandom::new()
            .fill(&mut random)
            .map_err(|_| PanelFailure)?;
        let token_key = hmac::Key::new(hmac::HMAC_SHA256, &random);
        let token_tag = hmac::sign(&token_key, config.token.as_bytes());
        let shared = Arc::new(Shared {
            service,
            authority: address.to_string(),
            token_key,
            token_tag,
            open: AtomicBool::new(true),
            slots: Semaphore::new(16),
        });
        let router = Router::new()
            .route("/", get(index))
            .route("/app.js", get(script))
            .route("/style.css", get(styles))
            .route("/api/status", get(status))
            .route("/api/sessions", get(sessions))
            .route("/api/tasks", get(tasks))
            .route("/api/session", post(session))
            .route("/api/cancel", post(cancel))
            .route("/api/judgments", get(judgments))
            .route("/api/goals", get(goals))
            .route("/api/goal", get(goal))
            .route("/api/memory/scopes", post(memory_scopes))
            .route("/api/memory/scope", post(memory))
            .route("/api/memory/evidence", post(memory_evidence))
            .fallback(|| async { error(StatusCode::NOT_FOUND, "not_found") })
            .layer(DefaultBodyLimit::max(16384))
            .layer(middleware::from_fn_with_state(shared.clone(), protect))
            .with_state(shared.clone());
        let (stop, mut stopped) = watch::channel(false);
        let (finished_sender, finished) = watch::channel(false);
        let task_shared = shared.clone();
        let task = tokio::spawn(async move {
            let result = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    while !*stopped.borrow_and_update() {
                        if stopped.changed().await.is_err() {
                            break;
                        }
                    }
                })
                .await;
            task_shared.open.store(false, Ordering::Release);
            finished_sender.send_replace(true);
            result
        });
        Ok(Self {
            address,
            shared,
            stop,
            finished,
            task: Some(task),
        })
    }
    pub fn address(&self) -> SocketAddr {
        self.address
    }
    /// 宿主观察异常退出；面板从不直接停止 Kernel 或 QQ。
    pub fn finished(&self) -> watch::Receiver<bool> {
        self.finished.clone()
    }
    pub fn request_stop(&self) {
        self.shared.open.store(false, Ordering::Release);
        self.stop.send_replace(true);
    }
    pub async fn stop(mut self) -> Result<(), PanelFailure> {
        self.request_stop();
        let mut task = self.task.take().ok_or(PanelFailure)?;
        match tokio::time::timeout(Duration::from_secs(6), &mut task).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(_) => Err(PanelFailure),
            Err(_) => {
                // 只终止本面板持有的服务器 Future；不发送系统信号、不操作其他服务。
                task.abort();
                let _ = task.await;
                Err(PanelFailure)
            }
        }
    }
}
impl Drop for LocalPanel {
    fn drop(&mut self) {
        self.request_stop();
    }
}

fn error(status: StatusCode, code: &'static str) -> Response {
    (status, Json(json!({"error":code}))).into_response()
}
fn failure(error_value: PanelError) -> Response {
    match error_value {
        PanelError::InvalidInput => error(StatusCode::BAD_REQUEST, "invalid_input"),
        PanelError::NotFound => error(StatusCode::NOT_FOUND, "not_found"),
        PanelError::Stale => error(StatusCode::CONFLICT, "stale_generation"),
        PanelError::Unavailable => error(StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
    }
}
fn headers(mut response: Response) -> Response {
    for (name, value) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
        ("referrer-policy", "no-referrer"),
        (
            "content-security-policy",
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'",
        ),
    ] {
        response
            .headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }
    response
}
async fn protect(State(shared): State<Arc<Shared>>, request: Request, next: Next) -> Response {
    let response = async {
        if !shared.open.load(Ordering::Acquire) {
            return error(StatusCode::SERVICE_UNAVAILABLE, "stopping");
        }
        let Ok(_slot) = shared.slots.try_acquire() else {
            return error(StatusCode::TOO_MANY_REQUESTS, "busy");
        };
        let incoming = request.headers();
        if incoming.get_all(header::HOST).iter().count() != 1
            || incoming
                .get(header::HOST)
                .and_then(|value| value.to_str().ok())
                != Some(shared.authority.as_str())
        {
            return error(StatusCode::FORBIDDEN, "invalid_host");
        }
        if incoming.get_all(header::ORIGIN).iter().count() > 1
            || incoming.get(header::ORIGIN).is_some_and(|value| {
                value.to_str().ok() != Some(format!("http://{}", shared.authority).as_str())
            })
            || incoming
                .get("sec-fetch-site")
                .is_some_and(|value| value == "cross-site")
        {
            return error(StatusCode::FORBIDDEN, "invalid_origin");
        }
        if request.uri().path().starts_with("/api/") {
            if incoming.get_all(header::AUTHORIZATION).iter().count() != 1 {
                return error(StatusCode::UNAUTHORIZED, "unauthorized");
            }
            let valid = incoming
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
                .filter(|value| (32..=256).contains(&value.len()))
                .is_some_and(|value| {
                    hmac::verify(
                        &shared.token_key,
                        value.as_bytes(),
                        shared.token_tag.as_ref(),
                    )
                    .is_ok()
                });
            if !valid {
                return error(StatusCode::UNAUTHORIZED, "unauthorized");
            }
            if request.method() == axum::http::Method::POST
                && incoming
                    .get(header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.split(';').next())
                    .map(str::trim)
                    != Some("application/json")
            {
                return error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "json_required");
            }
        }
        match tokio::time::timeout(Duration::from_secs(5), next.run(request)).await {
            Ok(response) => response,
            Err(_) => error(StatusCode::REQUEST_TIMEOUT, "request_timeout"),
        }
    }
    .await;
    headers(response)
}
async fn index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("../static/index.html"),
    )
}
async fn script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../static/app.js"),
    )
}
async fn styles() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../static/style.css"),
    )
}
async fn status(State(shared): State<Arc<Shared>>) -> Response {
    match shared.service.status() {
        Ok(value) => Json(value).into_response(),
        Err(e) => failure(e),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Listing {
    after: Option<String>,
    #[serde(default = "page_size")]
    limit: usize,
}
fn page_size() -> usize {
    25
}
fn valid_page(page: &Listing) -> bool {
    (1..=100).contains(&page.limit)
        && page.after.as_deref().is_none_or(|cursor| {
            !cursor.is_empty() && cursor.len() <= 256 && !cursor.chars().any(char::is_control)
        })
}
async fn sessions(
    State(shared): State<Arc<Shared>>,
    query: Result<Query<Listing>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Ok(Query(page)) = query else {
        return error(StatusCode::BAD_REQUEST, "invalid_query");
    };
    if !valid_page(&page) {
        return error(StatusCode::BAD_REQUEST, "invalid_query");
    }
    match shared.service.sessions(page.after.as_deref(), page.limit) {
        Ok(page) => {
            Json(json!({"sessions":page.items,"next_cursor":page.next_cursor})).into_response()
        }
        Err(e) => failure(e),
    }
}
async fn tasks(
    State(shared): State<Arc<Shared>>,
    query: Result<Query<Listing>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Ok(Query(page)) = query else {
        return error(StatusCode::BAD_REQUEST, "invalid_query");
    };
    if !valid_page(&page) {
        return error(StatusCode::BAD_REQUEST, "invalid_query");
    }
    match shared.service.tasks(page.after.as_deref(), page.limit) {
        Ok(page) => {
            Json(json!({"tasks":page.items,"next_cursor":page.next_cursor})).into_response()
        }
        Err(e) => failure(e),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Detail {
    key: SessionKey,
    before: Option<u64>,
    #[serde(default = "page_size")]
    limit: usize,
}
async fn session(
    State(shared): State<Arc<Shared>>,
    body: Result<Json<Detail>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error(StatusCode::BAD_REQUEST, "invalid_json");
    };
    if body.key.validate().is_err() || !(1..=50).contains(&body.limit) || body.before == Some(0) {
        return error(StatusCode::BAD_REQUEST, "invalid_input");
    }
    match shared.service.session(&body.key, body.before, body.limit) {
        Ok(value) => Json(value).into_response(),
        Err(e) => failure(e),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cancel {
    target: GenerationKey,
}
async fn cancel(
    State(shared): State<Arc<Shared>>,
    body: Result<Json<Cancel>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error(StatusCode::BAD_REQUEST, "invalid_json");
    };
    if body.target.session.validate().is_err()
        || body.target.generation == 0
        || body.target.task_id.is_empty()
        || body.target.task_id.len() > 256
        || body.target.task_id.trim() != body.target.task_id
        || body.target.task_id.chars().any(char::is_control)
    {
        return error(StatusCode::BAD_REQUEST, "invalid_input");
    }
    // 请求可能在读取正文时遇到宿主停止；不让旧连接在准入关闭后提交新动作。
    if !shared.open.load(Ordering::Acquire) {
        return error(StatusCode::SERVICE_UNAVAILABLE, "stopping");
    }
    match shared.service.cancel(&body.target) {
        Ok(status) => (StatusCode::ACCEPTED, Json(json!({"status": status}))).into_response(),
        Err(e) => failure(e),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JudgmentListing {
    before: Option<u64>,
    #[serde(default = "page_size")]
    limit: usize,
}
async fn judgments(
    State(shared): State<Arc<Shared>>,
    query: Result<Query<JudgmentListing>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Ok(Query(page)) = query else {
        return error(StatusCode::BAD_REQUEST, "invalid_query");
    };
    if !(1..=50).contains(&page.limit) || page.before == Some(0) {
        return error(StatusCode::BAD_REQUEST, "invalid_query");
    }
    match shared.service.judgments(page.before, page.limit) {
        Ok(value) => Json(value).into_response(),
        Err(e) => failure(e),
    }
}
async fn goals(
    State(shared): State<Arc<Shared>>,
    query: Result<Query<Listing>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Ok(Query(page)) = query else {
        return error(StatusCode::BAD_REQUEST, "invalid_query");
    };
    if !valid_page(&page) {
        return error(StatusCode::BAD_REQUEST, "invalid_query");
    }
    match shared.service.goals(page.after.as_deref(), page.limit) {
        Ok(value) => Json(value).into_response(),
        Err(e) => failure(e),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalQuery {
    id: String,
}
async fn goal(
    State(shared): State<Arc<Shared>>,
    query: Result<Query<GoalQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return error(StatusCode::BAD_REQUEST, "invalid_query");
    };
    // 与认知契约的 ID 规则一致；非法 ID 不进入宿主读取。
    if !valid_id(&query.id) {
        return error(StatusCode::BAD_REQUEST, "invalid_query");
    }
    match shared.service.goal(&query.id) {
        Ok(value) => Json(value).into_response(),
        Err(e) => failure(e),
    }
}
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && id.trim() == id && !id.chars().any(char::is_control)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryListing {
    after: Option<MemoryScope>,
    #[serde(default = "page_size")]
    limit: usize,
}
async fn memory_scopes(
    State(shared): State<Arc<Shared>>,
    body: Result<Json<MemoryListing>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error(StatusCode::BAD_REQUEST, "invalid_json");
    };
    if !(1..=100).contains(&body.limit)
        || body
            .after
            .as_ref()
            .is_some_and(|scope| scope.validate().is_err())
    {
        return error(StatusCode::BAD_REQUEST, "invalid_input");
    }
    match shared
        .service
        .memory_scopes(body.after.as_ref(), body.limit)
    {
        Ok(value) => Json(value).into_response(),
        Err(e) => failure(e),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryTarget {
    scope: MemoryScope,
}
async fn memory(
    State(shared): State<Arc<Shared>>,
    body: Result<Json<MemoryTarget>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error(StatusCode::BAD_REQUEST, "invalid_json");
    };
    if body.scope.validate().is_err() {
        return error(StatusCode::BAD_REQUEST, "invalid_input");
    }
    match shared.service.memory(&body.scope) {
        Ok(value) => Json(value).into_response(),
        Err(e) => failure(e),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceTarget {
    scope: MemoryScope,
    id: String,
}
async fn memory_evidence(
    State(shared): State<Arc<Shared>>,
    body: Result<Json<EvidenceTarget>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error(StatusCode::BAD_REQUEST, "invalid_json");
    };
    if body.scope.validate().is_err() || !valid_id(&body.id) {
        return error(StatusCode::BAD_REQUEST, "invalid_input");
    }
    match shared.service.memory_evidence(&body.scope, &body.id) {
        Ok(value) => Json(value).into_response(),
        Err(e) => failure(e),
    }
}
