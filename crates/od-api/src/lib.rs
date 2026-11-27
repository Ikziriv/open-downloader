//! Unified local HTTP control plane for Open Downloader:
//! - REST API v1 (`/api/v1/resolve`, `/api/v1/tasks`, pause/resume/delete)
//! - Real-time Server-Sent Events (`/api/v1/events`)
//! - Model Context Protocol JSON-RPC 2.0 endpoint (`/mcp`) for AI agents
//! - Backwards-compatible `POST /add` endpoint for the `od-nmh` browser host

mod mcp;

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::{self, Stream};
use od_core::autostart::{self, AutoStartStatus};
use od_core::{
    AppConfig, CancellationToken, CreateTaskRequest, Error, ResolveRequest, Resource,
    SpeedTestConfig, SpeedTestReport, TaskId, TaskManager, TaskPriority, TaskRecord,
    export_tasks_csv, run_speedtest, sanitize_filename,
};
use tokio::sync::RwLock;
use tokio::sync::broadcast::error::RecvError;

/// Shared state for the `od-api` Axum router.
#[derive(Clone)]
pub struct ApiState {
    pub manager: TaskManager,
    /// When `Some`, every protected route requires either `x-od-token: <tok>`
    /// or `Authorization: Bearer <tok>`.
    pub auth_token: Option<String>,
    /// Default destination directory when a request omits `dest`.
    pub default_dir: PathBuf,
    /// Protocol names reported by `/api/v1/health`.
    pub protocols: Vec<String>,
    /// Live application configuration.
    pub config: Arc<RwLock<AppConfig>>,
    /// Optional disk path where `PUT /api/v1/config` persists `config.json`.
    pub config_path: Option<PathBuf>,
}

impl ApiState {
    pub fn new(manager: TaskManager, default_dir: impl Into<PathBuf>) -> Self {
        let dir = default_dir.into();
        let mut cfg = AppConfig::default();
        cfg.downloads.download_dir = dir.clone();
        Self {
            manager,
            auth_token: None,
            default_dir: dir,
            protocols: vec!["http".into(), "https".into()],
            config: Arc::new(RwLock::new(cfg)),
            config_path: None,
        }
    }

    pub fn with_config(mut self, cfg: AppConfig, path: Option<PathBuf>) -> Self {
        self.default_dir = cfg.downloads.download_dir.clone();
        self.config = Arc::new(RwLock::new(cfg));
        self.config_path = path;
        self
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.auth_token = Some(token.into());
        self
    }

    pub fn with_protocols(mut self, protocols: Vec<String>) -> Self {
        self.protocols = protocols;
        self
    }
}

/// Build the full `od-api` Axum router (REST v1 + SSE + MCP + legacy `/add`).
pub fn router(state: ApiState) -> Router {
    let protected = Router::new()
        .route("/add", post(legacy_add))
        .route("/api/v1/resolve", post(resolve_handler))
        .route("/api/v1/speedtest", post(speedtest_handler))
        .route("/api/v1/ninja/inspect", post(ninja_inspect_handler))
        .route("/api/v1/system/downloaders", get(list_other_downloaders))
        .route(
            "/api/v1/system/downloaders/{id}/stop",
            post(stop_other_downloader),
        )
        .route("/api/v1/config", get(get_config).put(update_config))
        .route("/api/v1/tasks", get(list_tasks).post(create_task))
        .route("/api/v1/tasks/batch", post(create_tasks_batch))
        .route("/api/v1/tasks/export", get(export_tasks))
        .route("/api/v1/tasks/{id}", get(get_task).delete(remove_task))
        .route("/api/v1/tasks/{id}/pause", post(pause_task))
        .route("/api/v1/tasks/{id}/resume", post(resume_task))
        .route("/api/v1/events", get(events_sse))
        .route("/mcp", post(mcp::handle_mcp))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));

    Router::new()
        .route("/", get(dashboard_ui))
        .route("/api/v1/health", get(health))
        .merge(protected)
        .with_state(state)
}

async fn dashboard_ui() -> Html<&'static str> {
    Html(include_str!("ui.html"))
}

async fn require_auth(
    State(state): State<ApiState>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(expected) = &state.auth_token else {
        return Ok(next.run(req).await);
    };

    let header_ok = req
        .headers()
        .get("x-od-token")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t == expected);

    let bearer_ok = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| t == expected);

    if header_ok || bearer_ok {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

// ---------------------------------------------------------------------------
// API error mapping

#[derive(Debug, serde::Serialize)]
struct ErrorBody {
    error: String,
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(ErrorBody { error: self.1 })).into_response()
    }
}

impl From<Error> for ApiError {
    fn from(err: Error) -> Self {
        let code = match &err {
            Error::TaskNotFound(_) => StatusCode::NOT_FOUND,
            Error::InvalidState(_) | Error::UnsupportedProtocol(_) => StatusCode::BAD_REQUEST,
            Error::Cancelled => StatusCode::CONFLICT,
            _ => StatusCode::BAD_GATEWAY,
        };
        ApiError(code, err.to_string())
    }
}

// ---------------------------------------------------------------------------
// Health

#[derive(serde::Serialize)]
struct HealthResponse {
    ok: bool,
    version: &'static str,
    protocols: Vec<String>,
}

async fn health(State(state): State<ApiState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        version: env!("CARGO_PKG_VERSION"),
        protocols: state.protocols.clone(),
    })
}

// ---------------------------------------------------------------------------
// Resolve

#[derive(serde::Serialize)]
struct ResolveResponse {
    request: ResolveRequest,
    resource: Resource,
}

async fn resolve_handler(
    State(state): State<ApiState>,
    Json(req): Json<ResolveRequest>,
) -> Result<Json<ResolveResponse>, ApiError> {
    let (request, resource) = state.manager.resolve(&req).await?;
    Ok(Json(ResolveResponse { request, resource }))
}

// ---------------------------------------------------------------------------
// Speed Test

async fn speedtest_handler(
    cfg: Option<Json<SpeedTestConfig>>,
) -> Result<Json<SpeedTestReport>, ApiError> {
    let cfg = cfg.map(|Json(c)| c).unwrap_or_default();
    let client = reqwest::Client::new();
    let report = run_speedtest(&client, &cfg, CancellationToken::new(), None).await?;
    Ok(Json(report))
}

#[derive(serde::Deserialize)]
struct NinjaInspectPayload {
    text: String,
    #[serde(default = "default_ninja_source")]
    source: String,
}

fn default_ninja_source() -> String {
    "clipboard".into()
}

async fn ninja_inspect_handler(
    State(state): State<ApiState>,
    Json(payload): Json<NinjaInspectPayload>,
) -> Json<Vec<od_core::DetectedUrl>> {
    let settings = state.config.read().await.ninja.clone();
    let mut ninja = od_core::NinjaDetective::default();
    Json(ninja.inspect_text(&payload.text, &payload.source, &settings))
}

async fn list_other_downloaders() -> Result<Json<Vec<od_core::DetectedDownloader>>, ApiError> {
    let list = tokio::task::spawn_blocking(od_core::scan_downloaders)
        .await
        .map_err(|e| Error::Worker(e.to_string()))?;
    Ok(Json(list))
}

#[derive(serde::Deserialize, Default)]
struct StopQuery {
    #[serde(default)]
    force: bool,
}

#[derive(serde::Serialize)]
struct StopResponse {
    id: String,
    stopped_pids: usize,
}

async fn stop_other_downloader(
    Path(id): Path<String>,
    Query(q): Query<StopQuery>,
) -> Result<Json<StopResponse>, ApiError> {
    let id_clone = id.clone();
    let stopped_pids =
        tokio::task::spawn_blocking(move || od_core::stop_downloader(&id_clone, q.force))
            .await
            .map_err(|e| Error::Worker(e.to_string()))??;
    Ok(Json(StopResponse { id, stopped_pids }))
}

// ---------------------------------------------------------------------------
// Configuration (`/api/v1/config`)

#[derive(serde::Serialize)]
struct ConfigResponse {
    config: AppConfig,
    autostart: Option<AutoStartStatus>,
}

async fn get_config(State(state): State<ApiState>) -> Json<ConfigResponse> {
    let config = state.config.read().await.clone();
    let autostart = autostart::status().ok();
    Json(ConfigResponse { config, autostart })
}

async fn update_config(
    State(state): State<ApiState>,
    Json(mut next): Json<AppConfig>,
) -> Result<Json<ConfigResponse>, ApiError> {
    next.normalize();
    if let Some(path) = &state.config_path {
        next.save_to(path)?;
    }
    {
        let mut guard = state.config.write().await;
        *guard = next.clone();
    }
    let autostart = autostart::status().ok();
    Ok(Json(ConfigResponse {
        config: next,
        autostart,
    }))
}

// ---------------------------------------------------------------------------
// Tasks

#[derive(Debug, serde::Deserialize)]
pub struct CreateTaskPayload {
    pub url: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub extra: serde_json::Value,
    #[serde(default)]
    pub resource: Option<Resource>,
    #[serde(default)]
    pub dest: Option<PathBuf>,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub connections: Option<usize>,
    #[serde(default)]
    pub rate_limit: Option<u64>,
    #[serde(default)]
    pub organize: Option<bool>,
    #[serde(default)]
    pub expected_sha256: Option<String>,
    #[serde(default)]
    pub mirrors: Vec<String>,
    #[serde(default)]
    pub priority: TaskPriority,
    #[serde(default = "default_true")]
    pub auto_start: bool,
}

fn default_true() -> bool {
    true
}

pub(crate) async fn create_task_from_payload(
    state: &ApiState,
    payload: CreateTaskPayload,
) -> Result<TaskRecord, Error> {
    let cfg = state.config.read().await.clone();
    let mut opts = cfg.to_download_options();
    if let Some(c) = payload.connections {
        opts.connections = c.clamp(1, 32);
    }
    if let Some(rl) = payload.rate_limit {
        opts.rate_limit = Some(rl);
    }
    if let Some(org) = payload.organize {
        opts.organize = org;
    }
    if let Some(sha) = payload.expected_sha256.filter(|s| !s.trim().is_empty()) {
        opts.expected_sha256 = Some(sha);
    }
    if !payload.mirrors.is_empty() {
        opts.mirrors = payload.mirrors;
    }

    let base_dir = payload
        .dest
        .unwrap_or_else(|| cfg.downloads.download_dir.clone());
    let (dest, explicit_path) = match payload.filename.as_deref().and_then(sanitize_filename) {
        Some(name) => (base_dir.join(name), true),
        None => (base_dir, false),
    };

    let req = CreateTaskRequest {
        request: ResolveRequest {
            url: payload.url,
            headers: payload.headers,
            extra: payload.extra,
        },
        resource: payload.resource,
        dest,
        explicit_path,
        options: opts,
        priority: payload.priority,
        auto_start: payload.auto_start,
    };

    state.manager.create(req).await
}

async fn create_task(
    State(state): State<ApiState>,
    Json(payload): Json<CreateTaskPayload>,
) -> Result<(StatusCode, Json<TaskRecord>), ApiError> {
    let rec = create_task_from_payload(&state, payload).await?;
    Ok((StatusCode::CREATED, Json(rec)))
}

#[derive(serde::Deserialize)]
struct BatchTasksPayload {
    #[serde(default)]
    urls: Vec<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    dest: Option<PathBuf>,
    #[serde(default = "default_true")]
    auto_start: bool,
}

#[derive(serde::Serialize)]
struct BatchTasksResponse {
    created: Vec<TaskRecord>,
    errors: Vec<(String, String)>,
}

async fn create_tasks_batch(
    State(state): State<ApiState>,
    Json(payload): Json<BatchTasksPayload>,
) -> Json<BatchTasksResponse> {
    let mut urls = payload.urls;
    if let Some(text) = payload.text {
        urls.extend(od_core::extractors::parse_batch_urls(&text));
    }
    let mut created = Vec::new();
    let mut errors = Vec::new();
    for url in urls {
        let p = CreateTaskPayload {
            url: url.clone(),
            headers: Vec::new(),
            extra: serde_json::Value::Null,
            resource: None,
            dest: payload.dest.clone(),
            filename: None,
            connections: None,
            rate_limit: None,
            organize: None,
            expected_sha256: None,
            mirrors: Vec::new(),
            priority: TaskPriority::Normal,
            auto_start: payload.auto_start,
        };
        match create_task_from_payload(&state, p).await {
            Ok(rec) => created.push(rec),
            Err(e) => errors.push((url, e.to_string())),
        }
    }
    Json(BatchTasksResponse { created, errors })
}

async fn list_tasks(State(state): State<ApiState>) -> Json<Vec<TaskRecord>> {
    Json(state.manager.list().await)
}

#[derive(serde::Deserialize)]
struct ExportQuery {
    #[serde(default = "default_export_format")]
    format: String,
}

fn default_export_format() -> String {
    "json".into()
}

async fn export_tasks(
    State(state): State<ApiState>,
    Query(q): Query<ExportQuery>,
) -> Response {
    let list = state.manager.list().await;
    if q.format.eq_ignore_ascii_case("csv") {
        let csv = export_tasks_csv(&list);
        (
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/csv; charset=utf-8",
            )],
            csv,
        )
            .into_response()
    } else {
        Json(list).into_response()
    }
}

async fn get_task(
    State(state): State<ApiState>,
    Path(id): Path<TaskId>,
) -> Result<Json<TaskRecord>, ApiError> {
    state
        .manager
        .get(id)
        .await
        .map(Json)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("task {id} not found")))
}

async fn pause_task(
    State(state): State<ApiState>,
    Path(id): Path<TaskId>,
) -> Result<StatusCode, ApiError> {
    state.manager.pause(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn resume_task(
    State(state): State<ApiState>,
    Path(id): Path<TaskId>,
) -> Result<StatusCode, ApiError> {
    state.manager.resume(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(serde::Deserialize)]
struct DeleteQuery {
    #[serde(default)]
    delete_files: bool,
}

async fn remove_task(
    State(state): State<ApiState>,
    Path(id): Path<TaskId>,
    Query(q): Query<DeleteQuery>,
) -> Result<StatusCode, ApiError> {
    state.manager.remove(id, q.delete_files).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Backwards-compatible `POST /add` for `od-nmh` browser native messaging host

#[derive(serde::Deserialize)]
struct LegacyAddPayload {
    url: String,
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    cookie: Option<String>,
    #[serde(default)]
    referer: Option<String>,
}

async fn legacy_add(
    State(state): State<ApiState>,
    Json(body): Json<LegacyAddPayload>,
) -> Result<Json<TaskRecord>, ApiError> {
    let mut headers = Vec::new();
    if let Some(c) = body.cookie.filter(|s| !s.is_empty()) {
        headers.push(("cookie".into(), c));
    }
    if let Some(r) = body.referer.filter(|s| !s.is_empty()) {
        headers.push(("referer".into(), r));
    }
    let rec = create_task_from_payload(
        &state,
        CreateTaskPayload {
            url: body.url,
            headers,
            extra: serde_json::Value::Null,
            resource: None,
            dest: None,
            filename: body.filename,
            connections: None,
            rate_limit: None,
            organize: Some(true),
            expected_sha256: None,
            mirrors: Vec::new(),
            priority: TaskPriority::Normal,
            auto_start: true,
        },
    )
    .await?;
    Ok(Json(rec))
}

// ---------------------------------------------------------------------------
// Server-Sent Events (`/api/v1/events`)

async fn events_sse(
    State(state): State<ApiState>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = state.manager.subscribe();
    let stream = stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let payload = serde_json::to_string(&ev).unwrap_or_else(|_| "{}".into());
                    return Some((Ok(SseEvent::default().data(payload)), rx));
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
