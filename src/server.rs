//! Headless HTTP server (`e-agent --serve`): axum 0.8 + SSE + SessionRegistry.
//!
//! Serves a single-file web UI at `/` (compiled in via `include_str!` once
//! `src/ui/index.html` exists; a minimal placeholder page before that) and a
//! token-authenticated `/api/*` surface for managing live sessions:
//!
//! | Method | Path                              | Semantics                          |
//! |--------|-----------------------------------|------------------------------------|
//! | GET    | `/api/sessions`                   | list active sessions               |
//! | POST   | `/api/sessions`                   | create a session                   |
//! | GET    | `/api/sessions/{id}/events`       | SSE: snapshot, then live events    |
//! | GET    | `/api/sessions/{id}/history`      | segmented history (head or older)  |
//! | GET    | `/api/sessions/{id}/summary`     | per-turn summary cache (desktop pet) |
//! | GET    | `/api/sessions/{id}/usage`        | persisted token usage (session + subagents) |
//! | POST   | `/api/sessions/{id}/prompt`       | queue a prompt                     |
//! | POST   | `/api/sessions/{id}/btw`          | fork into a persistent subagent    |
//! | GET    | `/api/sessions/{id}/fork-candidates` | turn boundaries to fork at (at/seq/preview) |
//! | POST   | `/api/sessions/{id}/fork`          | fork at a turn boundary into a `fork-…` session |
//! | POST   | `/api/sessions/{id}/cancel`       | cancel the in-flight turn          |
//! | POST   | `/api/sessions/{id}/compact`      | request compaction                 |
//! | GET    | `/api/models`                     | switchable model profile names (web `/model` autocomplete) |
//! | GET    | `/api/pet/config`                 | live desktop pet sprite settings |
//! | GET    | `/api/pet/sprite`                 | configured local sprite-sheet bytes |
//! | POST   | `/api/sessions/{id}/model`         | switch the session's model at runtime |
//! | POST   | `/api/sessions/{id}/undo`         | undo the most recent file operation |
//! | GET    | `/api/sessions/{id}/goal`         | current goal snapshot (or null)     |
//! | POST   | `/api/sessions/{id}/goal`         | create / pause / resume / clear the goal |
//! | PUT    | `/api/sessions/{id}/title`        | rename a session                  |
//! | DELETE | `/api/sessions/{id}`              | cancel + remove from the registry  |
//! | GET    | `/api/tasks`                        | running background tasks, all sessions |
//! | DELETE | `/api/sessions/{id}/tasks/{task_id}` | cancel one background task          |
//! | GET    | `/api/sessions/{id}/tasks/{task_id}/output` | full output of a running bash task |
//! | GET    | `/api/images/{hash}`               | content-addressed image bytes for `<img>` rendering |
//!
//! Authentication: a random token generated at startup (written to
//! `$XDG_STATE_HOME/e-agent/server.token` or `~/.local/state/e-agent/server.token`,
//! mode 0600) is required on every `/api/*` request as
//! `Authorization: Bearer <token>` or `?token=<token>`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};

use anyhow::{Context as AnyhowContext, anyhow};
use axum::body::Body;
use axum::extract::{Path, Query, State, rejection::JsonRejection};
use axum::http::{Method, Request, StatusCode, header};
use axum::middleware::{Next, from_fn, from_fn_with_state};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Error, Json, Router};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc, watch};

use crate::agent::{AgentEvent, Message, Model, SessionEntry, preview};
use crate::delegate::Sessions;
use crate::runner::{IdlePolicy, SessionHandle, SessionStatus, SessionTask};
use crate::session_factory::{SessionBuild, SessionFactory, UnfinishedPolicy};
use crate::session_store::{FinishedTask, ListMetaDiagnostics, SessionStore};
use crate::tools::{BackgroundTaskInfo, BackgroundTasks};

/// Heartbeat interval for SSE connections (comment line `: ping`).
const HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(15);

/// Bounded per-connection SSE queue. A stalled TCP peer cannot grow an
/// unbounded buffer (ToolResult payloads can be MB-sized): when the queue
/// fills, `forward_events` drops the connection and the frontend's 3s
/// reconnect logic re-establishes it.
const SSE_CHANNEL_CAPACITY: usize = 256;

/// Cap for the `snapshot` / `resync` event arrays. The in-memory event log
/// grows without bound on a long session; the initial snapshot is only a
/// fallback when the history route fails, and a resync only needs to cover
/// the broadcast gap (256) with margin, so the newest `SNAPSHOT_MAX` events
/// are enough to re-render a client.
const SNAPSHOT_MAX: usize = 1000;

/// Keep only the newest `SNAPSHOT_MAX` events of a log snapshot (drop the
/// oldest). Bounds the per-connection snapshot/resync frame size on long
/// sessions.
fn tail_snapshot(mut events: Vec<AgentEvent>) -> Vec<AgentEvent> {
    let len = events.len();
    if len > SNAPSHOT_MAX {
        events.drain(..len - SNAPSHOT_MAX);
    }
    events
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Startup
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

/// Human-readable session-backend type name for the startup log. Never
/// Debug-prints the enum: a Greptime conn string may embed credentials.
fn backend_name(backend: &crate::config::SessionBackend) -> &'static str {
    match backend {
        crate::config::SessionBackend::Jsonl => "jsonl",
        crate::config::SessionBackend::Greptime { .. } => "greptime",
        crate::config::SessionBackend::Sqlite { .. } => "sqlite",
    }
}

/// Short stable hash (FNV-1a 32-bit, hex) of the canonical workspace root —
/// a compact stand-in for the full `workspace_id` (which is the root path
/// itself, see `session_greptime::derive_workspace_id`). Lets a startup log
/// line cross-reference the backend's workspace_id column without repeating
/// a long path.
fn short_workspace_id(root: &std::path::Path) -> String {
    let bytes = root.to_string_lossy();
    let mut hash = 0x811c_9dc5u32;
    for &b in bytes.as_bytes() {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{hash:08x}")
}

#[cfg(test)]
fn killed_notice_text(own: &[String], killed_subagents: &[(String, usize)]) -> Option<String> {
    if own.is_empty() && killed_subagents.is_empty() {
        return None;
    }
    let n = own.len() + killed_subagents.iter().map(|(_, n)| n).sum::<usize>();
    let mut text = format!(
        "[e-agent exited with {n} background task(s) still running; they were killed with the process. Re-run them if still needed:]"
    );
    if !own.is_empty() {
        text.push('\n');
        text.push_str(&own.join("\n"));
    }
    if !killed_subagents.is_empty() {
        text.push('\n');
        text.push_str("被杀子会话: ");
        text.push_str(
            &killed_subagents
                .iter()
                .map(|(label, _)| label.as_str())
                .collect::<Vec<_>>()
                .join("、"),
        );
    }
    Some(text)
}

/// Bind, authenticate, and serve until Ctrl-C. The factory is resolved once
/// here and reused by every session `build()`.
pub async fn run(factory: SessionFactory, host: &str, port: u16) -> anyhow::Result<()> {
    // Config hot reload: a background task polls the config files and, on a
    // valid change, atomically swaps the factory's config so new sessions
    // and `/model` switches pick up edited `[models]`/`[providers]`/`[roles]`
    // without restarting the server.
    factory.spawn_config_watcher();
    let token = load_or_create_token()?;
    // Metadata storage is opened lazily by request handlers. Startup must be
    // storage-free: connecting SQLite/Greptime can run migrations/DDL.
    let state = Arc::new(AppState {
        factory,
        registry: Arc::new(SessionRegistry::default()),
        token,
        #[cfg(test)]
        meta_store: SessionStore::Jsonl,
        summaries: Arc::new(Mutex::new(HashMap::new())),
        summary_pending: Arc::new(SummaryPending(Mutex::new(HashSet::new()))),
        shutdown: watch::channel(()).0,
    });
    eprintln!(
        "e-agent: serving on http://{host}:{port} (token: {}; also at {})",
        state.token,
        token_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<no state dir>".to_owned())
    );

    // 启动可观测性：canonical workspace root、backend 类型、workspace_id
    // 短 hash、PID。workspace_id 由 canonical root 派生（见
    // session_greptime::derive_workspace_id），短 hash 便于多实例日志对照；
    // 排查「响应来自哪台实例」时不用再对着长路径数。
    eprintln!(
        "e-agent: workspace {} backend {} workspace_id {} pid {}",
        state.factory.root().display(),
        backend_name(state.factory.backend()),
        short_workspace_id(state.factory.root()),
        std::process::id()
    );
    let listener = tokio::net::TcpListener::bind((host, port))
        .await
        .with_context(|| format!("cannot bind {host}:{port}"))?;
    // Ctrl-C 触发 graceful shutdown：axum 停 accept，放行 in-flight 请求收尾
    // （持久化写入在 handler 响应前已同步落盘，drain 截断不影响数据安全）。
    // 但 graceful shutdown 会无限等待 in-flight 连接——浏览器标签页挂着的
    // /api/sessions/{id}/events SSE 流永不结束（15s ping 保活），所以给整个
    // serve future 一条"从 Ctrl-C 起算"的硬截止线：Ctrl-C 立刻停 accept 并
    // 开始 drain（idle 连接秒关），drain 自然完成就立即退出；超过
    // SHUTDOWN_DRAIN_TIMEOUT 仍未完成则强制退出。第二次 Ctrl-C 仍由默认
    // handler 直接强杀。方案 B：Ctrl-C 同时通知 SSE 流自关闭（见 AppState
    // 的 shutdown watch），让 drain 毫秒级完成，2s 截止线退化为纯兜底。
    let (drain_started_tx, drain_started_rx) = tokio::sync::oneshot::channel::<()>();
    let shutdown_tx = state.shutdown.clone();
    let shutdown = async move {
        shutdown_signal().await; // Ctrl-C
        let _ = shutdown_tx.send(()); // 方案 B：SSE 流自感知 shutdown
        let _ = drain_started_tx.send(()); // 硬截止线从现在开始计时
    };
    // `with_graceful_shutdown` 只实现 `IntoFuture`（非 `Future`），先转换为
    // 真正的 future 再 pin，这样 select 的两个分支都能 `&mut serve` 轮询。
    let serve = axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .into_future();
    tokio::pin!(serve);
    tokio::select! {
        // drain 自然完成（没有 in-flight / SSE 已自关闭）→ 正常退出。
        result = &mut serve => Ok(result.context("server error")?),
        // Ctrl-C 已按下：给 in-flight 请求至多 SHUTDOWN_DRAIN_TIMEOUT 收尾。
        // 提前完成立即退出；到期强制退出——挂着的 SSE 流也不能无限拖住进程。
        _ = drain_started_rx => {
            match tokio::time::timeout(SHUTDOWN_DRAIN_TIMEOUT, &mut serve).await {
                Ok(result) => result.context("server error")?,
                Err(_elapsed) => {} // 到期：强制退出
            }
            Ok(())
        }
    }
}

/// Ctrl-C 后允许 drain 的硬上限：graceful shutdown 开始后，in-flight 请求
/// 至多再享有这么多时间收尾，到期进程强制退出。持久化在 handler 响应前已
/// 同步落盘，所以这只覆盖一个卡住的 handler / 挂着的 SSE 流，不涉及数据
/// 持久性。
const SHUTDOWN_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

async fn shutdown_signal() {
    // Ctrl-C only. A second Ctrl-C kills the process outright (default
    // handler).
    let _ = tokio::signal::ctrl_c().await;
}

/// One cached session summary: the one-sentence Chinese text generated at
/// the end of the most recent turn with real activity, plus the time it was
/// generated. Written by the per-session turn-end listener, read by the
/// desktop pet. Entries are never evicted: a live session has at most one,
/// and a stale-but-bounded cache is fine for a click-time display.
#[derive(Clone)]
pub struct SummaryEntry {
    pub text: String,
    pub at: std::time::SystemTime,
}

/// Per-session "summary generation in flight" set: the desktop pet's
/// on-demand path triggers one background generation when the cache is
/// cold; this set prevents a click storm from piling up duplicate
/// generation calls.
pub struct SummaryPending(pub std::sync::Mutex<std::collections::HashSet<String>>);

/// Everything the handlers need: the shared factory, the live-session
/// registry, the startup token, and the per-session summary cache.
pub struct AppState {
    pub factory: SessionFactory,
    pub registry: Arc<SessionRegistry>,
    pub token: String,
    /// Test-only compatibility field for direct router unit fixtures. Production
    /// handlers always obtain the configured backend through `meta_store()`.
    #[cfg(test)]
    pub meta_store: SessionStore,
    /// session_id -> (总结文本, 生成时间戳): written at the end of every
    /// turn with real activity, read by the desktop pet via
    /// `GET /api/sessions/{id}/summary`. Read-mostly, so a plain
    /// `Mutex<HashMap>` suffices.
    pub summaries: Arc<Mutex<HashMap<String, SummaryEntry>>>,
    /// session_id -> in-flight on-demand generation (desktop pet click on a
    /// cold cache): prevents duplicate generation calls.
    pub summary_pending: Arc<SummaryPending>,
    /// 服务器 shutdown 信号（方案 B）：Ctrl-C 时 `send(())`，SSE 流
    /// (`forward_events`) 收到后立即自关闭，连接随之关闭，graceful drain
    /// 毫秒级完成——`SHUTDOWN_DRAIN_TIMEOUT` 硬截止线因此只是兜底。
    /// 每个 SSE 连接通过 `subscribe()` 取自己的接收端。
    pub shutdown: watch::Sender<()>,
}

async fn meta_store(state: &AppState) -> anyhow::Result<SessionStore> {
    match state.factory.backend() {
        crate::config::SessionBackend::Jsonl => Ok(SessionStore::Jsonl),
        _ => SessionStore::connect_meta(state.factory.backend(), state.factory.root()).await,
    }
}

fn router(state: Arc<AppState>) -> Router {
    let api = Router::new()
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/{id}/events", get(session_events))
        .route("/api/sessions/{id}/history", get(session_history))
        .route("/api/sessions/{id}/summary", get(session_summary))
        .route("/api/sessions/{id}/usage", get(session_usage))
        .route("/api/sessions/{id}/prompt", post(session_prompt))
        .route("/api/sessions/{id}/btw", post(session_btw))
        .route("/api/sessions/{id}/fork-candidates", get(fork_candidates))
        .route("/api/sessions/{id}/fork", post(session_fork))
        .route("/api/sessions/{id}/cancel", post(session_cancel))
        .route("/api/sessions/{id}/compact", post(session_compact))
        .route("/api/models", get(list_models))
        .route("/api/pet/config", get(pet_config))
        .route("/api/pet/sprite", get(serve_pet_sprite))
        .route("/api/sessions/{id}/model", post(session_model))
        .route("/api/sessions/{id}/undo", post(session_undo))
        .route(
            "/api/sessions/{id}/goal",
            get(session_goal_get).post(session_goal_post),
        )
        .route("/api/sessions/{id}/title", put(session_title))
        .route("/api/sessions/{id}/pin", put(session_pin))
        .route("/api/sessions/{id}/archive", put(session_archive))
        .route("/api/sessions/{id}", delete(delete_session))
        .route("/api/tasks", get(list_tasks))
        .route("/api/tasks/finished", get(finished_tasks))
        .route("/api/sessions/{id}/tasks/{task_id}", delete(cancel_task))
        .route(
            "/api/sessions/{id}/tasks/{task_id}/output",
            get(task_output),
        )
        .route("/api/images/{hash}", get(serve_image))
        .route_layer(from_fn_with_state(state.clone(), require_auth))
        .layer(from_fn(cache_control_middleware));
    Router::new()
        .route("/", get(index))
        .merge(api)
        .with_state(state)
        .layer(from_fn(cors_middleware))
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// CORS
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

/// CORS middleware: lets the browser UI served by one server reach the API
/// of another server (multi-workspace mode). `*` origin is acceptable here
/// because the API token is never auto-attached cross-origin by the browser
/// (fetch defaults to same-origin credentials) — only a client that
/// presents the Bearer token can read a response, and that gate is
/// untouched. Preflight (`OPTIONS`) is answered with the headers the
/// frontend needs (`Authorization` / `Content-Type`); all other requests
/// pass through and get `Access-Control-Allow-Origin` on their response so
/// the browser surfaces real status codes (401 etc.) instead of a generic
/// CORS failure.
async fn cors_middleware(request: Request<Body>, next: Next) -> Response {
    if request.method() == Method::OPTIONS {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NO_CONTENT;
        let headers = response.headers_mut();
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            header::HeaderValue::from_static("*"),
        );
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            header::HeaderValue::from_static("GET, POST, PUT, DELETE, OPTIONS"),
        );
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            header::HeaderValue::from_static("Authorization, Content-Type"),
        );
        headers.insert(
            header::ACCESS_CONTROL_MAX_AGE,
            header::HeaderValue::from_static("86400"),
        );
        return response;
    }
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        header::HeaderValue::from_static("*"),
    );
    response
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Cache control
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

/// Cache-control middleware for every dynamic `/api/*` response. `no-store`
/// forbids the browser from serving a stale API response out of its HTTP
/// cache — the exact class of bug that mislabeled pinned sessions across
/// workspaces (a browser-held response from an old instance / old connection
/// kept being re-read after the instance died). The static UI already sends
/// its own `no-store`; this only touches the API sub-router. Preflight
/// (`OPTIONS`) is answered by `cors_middleware` before reaching here and
/// carries no data, so it needs no cache header.
async fn cache_control_middleware(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Token
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

/// `$XDG_STATE_HOME/e-agent` or `~/.local/state/e-agent`, mirroring the
/// XDG resolution used by `config::config_dir()` for the config tree.
pub fn state_dir_inner(
    xdg: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if let Some(xdg) = xdg
        && !xdg.is_empty()
    {
        Some(PathBuf::from(xdg).join("e-agent"))
    } else {
        home.map(|home| PathBuf::from(home).join(".local/state/e-agent"))
    }
}

fn state_dir() -> Option<PathBuf> {
    state_dir_inner(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        crate::home_dir().as_deref().map(std::path::Path::as_os_str),
    )
}

fn token_path() -> Option<PathBuf> {
    state_dir().map(|dir| dir.join("server.token"))
}

/// Load the server token, generating and persisting one only when absent.
/// The token is reused across restarts so clients (browser localStorage)
/// keep working; it is written with mode 0600 on Unix.
pub fn load_or_create_token() -> anyhow::Result<String> {
    let path = token_path()
        .ok_or_else(|| anyhow!("cannot resolve server token path: no XDG_STATE_HOME or HOME"))?;
    load_or_create_token_at(path)
}

fn load_or_create_token_at(path: PathBuf) -> anyhow::Result<String> {
    let dir = path.parent().expect("token path always has a parent");
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    // Reuse an existing token across restarts; generate only when absent
    // or empty (a previous server's clients stay valid).
    if let Ok(contents) = std::fs::read_to_string(&path) {
        let token = contents.trim();
        if !token.is_empty() {
            return Ok(token.to_string());
        }
    }
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut bytes);
    let token = base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("create {}", path.display()))?;
    std::io::Write::write_all(&mut file, token.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    Ok(token)
}

/// Accepts `Authorization: Bearer <token>` or `?token=<token>` (the query
/// fallback exists for EventSource-style clients that cannot set headers).
fn authorized(token: &str, request: &axum::extract::Request) -> bool {
    if let Some(value) = request.headers().get(header::AUTHORIZATION)
        && let Ok(value) = value.to_str()
        && let Some(bearer) = value.strip_prefix("Bearer ")
        && bearer == token
    {
        return true;
    }
    if let Some(query) = request.uri().query() {
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            if key == "token" && value == token {
                return true;
            }
        }
    }
    false
}

async fn require_auth(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if authorized(&state.token, &request) {
        next.run(request).await
    } else {
        (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response()
    }
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Session registry
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

/// One live web session: the runner handle + task and everything the frontend
/// needs to render metadata. The registry key (the session id) is the source
/// of truth for the id; it is not duplicated here.
pub struct LiveSession {
    pub handle: SessionHandle,
    /// `runner.start(None)` task; dropped (aborting the runner) when the
    /// session is deleted or the process exits.
    pub task: SessionTask,
    pub store: SessionStore,
    pub background: BackgroundTasks,
    /// Live subagent session registry (background delegates).
    pub sessions: Sessions,
    /// Display model name; mutable so a runtime `/model` switch is
    /// reflected in `GET /api/sessions` without rebuilding the session.
    model_name: Mutex<String>,
    pub role_name: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl LiveSession {
    pub fn model_name(&self) -> String {
        self.model_name.lock().unwrap().clone()
    }

    pub fn set_model_name(&self, name: String) {
        *self.model_name.lock().unwrap() = name;
    }
}

/// Active-session registry, keyed by session id. Observers are not tracked
/// here: the runner's broadcast channel already gives every SSE subscriber
/// an atomic `attach()` (snapshot + live + status with no gap), so a shared
/// fanout list would duplicate that mechanism ("one adapter, no seam").
#[derive(Default)]
struct RegistryInner {
    live: HashMap<String, Arc<LiveSession>>,
    creating: HashSet<String>,
}

#[derive(Default)]
pub struct SessionRegistry {
    inner: Mutex<RegistryInner>,
}

pub struct SessionReservation {
    registry: Arc<SessionRegistry>,
    id: Option<String>,
}

impl SessionRegistry {
    pub fn reserve(self: &Arc<Self>, id: &str) -> Result<SessionReservation, bool> {
        let mut inner = self.inner.lock().unwrap();
        if inner.live.contains_key(id) || !inner.creating.insert(id.to_owned()) {
            return Err(inner.live.contains_key(id));
        }
        Ok(SessionReservation {
            registry: self.clone(),
            id: Some(id.to_owned()),
        })
    }

    pub fn insert(&self, id: String, session: Arc<LiveSession>) {
        let mut inner = self.inner.lock().unwrap();
        inner.creating.remove(&id);
        inner.live.insert(id, session);
    }

    pub fn get(&self, id: &str) -> Option<Arc<LiveSession>> {
        self.inner.lock().unwrap().live.get(id).cloned()
    }

    pub fn remove(&self, id: &str) -> Option<Arc<LiveSession>> {
        self.inner.lock().unwrap().live.remove(id)
    }

    /// Snapshot of live `(id, session)` pairs; creating sessions are omitted.
    pub fn list(&self) -> Vec<(String, Arc<LiveSession>)> {
        self.inner
            .lock()
            .unwrap()
            .live
            .iter()
            .map(|(id, session)| (id.clone(), session.clone()))
            .collect()
    }
}

impl SessionReservation {
    pub fn publish(mut self, session: Arc<LiveSession>) {
        let id = self
            .id
            .take()
            .expect("session reservation already published");
        let mut inner = self.registry.inner.lock().unwrap();
        inner.creating.remove(&id);
        inner.live.insert(id, session);
    }
}

impl Drop for SessionReservation {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.registry.inner.lock().unwrap().creating.remove(&id);
        }
    }
}

/// A session the web can address by id: either a live registry session or a
/// live subagent session (delegate/btw) registered in some parent's
/// [`Sessions`] registry. Subagents are keyed by background-task id there,
/// not by session id, so they never land in the main registry — `live()`
/// resolves both so every session endpoint (history/SSE/prompt/cancel/
/// compact) works on a subagent exactly as it does on a main session.
///
/// Design choice (vs. wrapping a `SessionEntry` into an `Arc<LiveSession>`):
/// the web-facing surface of a subagent is just its handle + session-bound
/// store + display fields; `LiveSession` additionally carries a runner task,
/// a `BackgroundTasks` registry and a `Sessions` registry that a subagent
/// does not own, so padding it would be dead weight and would invite
/// pretending a subagent has a background-task registry it does not have
/// (task cancellation stays addressed to the *parent* session).
pub enum SessionRef {
    /// A session in the main `SessionRegistry` (key = session id).
    Live(Arc<LiveSession>),
    /// A subagent registered in a parent's `Sessions` registry
    /// (key = background-task id, `entry.session_id` is the address).
    /// The parent and task id are kept so `DELETE /api/sessions/{id}` can
    /// truly terminate the subagent through the parent's
    /// `BackgroundTasks::cancel(task_id)` — a plain `handle().cancel()`
    /// only interrupts the current turn and must not be relied on to end a
    /// delegate.
    Subagent {
        entry: Arc<crate::delegate::SessionEntry>,
        parent: Arc<LiveSession>,
        task_id: u64,
    },
}

impl SessionRef {
    /// The runner handle both variants carry. Prompt/cancel/compact/SSE
    /// attach all go through it; history additionally needs the
    /// session-bound store (`LiveSession.store` / `SessionEntry.store`).
    fn handle(&self) -> SessionHandle {
        match self {
            SessionRef::Live(session) => session.handle.clone(),
            SessionRef::Subagent { entry, .. } => entry.handle.clone(),
        }
    }
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Wire DTOs
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

/// Frontend status string: CamelCase, exactly the values the UI compares
/// against (`statusLabel` / `statusChipClass` / `applyStatus` compare
/// `=== "Busy"` etc.). Finished details are intentionally omitted: the
/// frontend renders the Finished chip from the bare string and never reads
/// the result payload.
fn status_string(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Idle => "Idle",
        SessionStatus::Busy => "Busy",
        SessionStatus::Compacting => "Compacting",
        SessionStatus::Finished(_) => "Finished",
    }
}

/// Wire payload of `event: status` SSE frames. The frontend does
/// `applyStatus(JSON.parse(data).status)`, so the frame must be an object
/// with a `status` key carrying the CamelCase string (not a bare string).
fn status_json(status: &SessionStatus) -> serde_json::Value {
    serde_json::json!({ "status": status_string(status) })
}

/// Session metadata for `GET /api/sessions` and `POST /api/sessions`.
#[derive(Serialize)]
pub struct SessionMeta {
    pub id: String,
    pub model: String,
    pub role: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Last activity time (session start / last tool call / last message);
    /// the session list sorts by this descending within the pinned group so
    /// the most recently active session renders first (pinned sessions sort
    /// ahead of unpinned ones).
    pub last_active_at: chrono::DateTime<chrono::Utc>,
    /// CamelCase frontend status string (`"Idle" | "Busy" | "Compacting" |
    /// "Finished"`); the UI's `statusLabel`/`statusChipClass`/`applyStatus`
    /// compare against these exact values.
    pub status: String,
    /// Number of persisted `SessionEntry` values (the list renders "N 条").
    pub entry_count: usize,
    /// True while a turn is in flight (Busy or Compacting); the list renders
    /// the busy dot from this.
    pub busy: bool,
    /// True while the session is live: either in the main registry, or —
    /// for subagents — registered in a live parent's `Sessions` registry
    /// (real handles; `list_sessions` backfills those). Historical sessions
    /// from the metadata table are `false` and rendered grey by the
    /// frontend; clicking one resumes it (`POST /api/sessions {id}`). The
    /// running_tasks label lookup never sets this — the label is task
    /// metadata, not runtime liveness.
    pub active: bool,
    /// Parent subagent link (sessions spawned by the `delegate` tool); the
    /// frontend shows it as provenance when present.
    pub parent_session_id: Option<String>,
    /// User-assigned session name (manual, never auto-generated). `None` =
    /// unnamed — the frontend shows the session id. Registry sessions are
    /// built from the metadata table via [`merge_session_metas`] (the
    /// registry DTO itself carries no title), so live renames show up on
    /// the next list.
    pub title: Option<String>,
    /// User pin flag: `Some(true)` = pinned (sorted first in the list),
    /// `Some(false)` / `None` = unpinned. Registry sessions are built from
    /// the metadata table via [`merge_session_metas`] (the registry DTO
    /// itself carries no pin), so live pins show up on the next list.
    pub pinned: Option<bool>,
    /// User archive flag: `Some(true)` = archived (hidden from the default
    /// session list, folded into the sidebar's collapsed "归档" group),
    /// `Some(false)` / `None` = unarchived. Registry sessions are built
    /// from the metadata table via [`merge_session_metas`] (the registry
    /// DTO itself carries no archive flag), so live archives show up on
    /// the next list.
    pub archived: Option<bool>,
    /// Task-panel label of the delegate task that spawned this subagent
    /// (`running_tasks.label`, newest surviving row — the sessions metadata
    /// table does not store it). `None` = no live delegate task carries
    /// this session; the frontend falls back to the session id. Filled by
    /// `list_sessions` after the merge; `None` everywhere else.
    pub label: Option<String>,
}

async fn session_meta(id: &str, session: &LiveSession, root: &std::path::Path) -> SessionMeta {
    let status = session.handle.status();
    let status = status.borrow().clone();
    // Entry count WITHOUT loading/parsing the whole transcript: JSONL
    // line-counts the file on a blocking thread (a full `load` per live
    // session per poll is the sync parse that blocked the executor);
    // Greptime/SQLite read the store's in-memory next_seq (no DB query).
    let entry_count = session
        .store
        .count_entries(root, id)
        .await
        .map(|count| count.max(0) as usize)
        .unwrap_or(0);
    SessionMeta {
        id: id.to_owned(),
        model: session.model_name(),
        role: session.role_name.clone(),
        created_at: session.created_at,
        // Registry sessions are live right now, so "last active" is the
        // present moment; the list sort puts them at the top.
        last_active_at: chrono::Utc::now(),
        status: status_string(&status).to_owned(),
        entry_count,
        busy: matches!(status, SessionStatus::Busy | SessionStatus::Compacting),
        active: true,
        parent_session_id: None,
        // The registry DTO has no title; the merge fills it from the
        // session's metadata-table snapshot (a built session always has
        // one — `build` → `create_meta`).
        title: None,
        // The registry DTO has no pin; the merge fills it from the
        // session's metadata-table snapshot like the title.
        pinned: None,
        // The registry DTO has no archive flag; the merge fills it from
        // the session's metadata-table snapshot like the pin.
        archived: None,
        // The label lives in running_tasks; `list_sessions` fills it for
        // subagent items after the merge.
        label: None,
    }
}

fn error(status: StatusCode, message: impl Into<String>) -> (StatusCode, String) {
    (status, message.into())
}

/// One running background task, flattened for the wire. Task ids are only
/// unique per session (`BackgroundTasks` ids increment per registry), so
/// every entry carries its `session_id` and clients address a task as
/// `/api/sessions/{session_id}/tasks/{id}`.
#[derive(Serialize)]
pub struct TaskMeta {
    pub session_id: String,
    pub id: u64,
    pub label: String,
    pub full_command: Option<String>,
    pub role: Option<String>,
    /// `"bash"` for background shell commands, `"delegate"` for subagents.
    pub kind: String,
    /// Lossy UTF-8 of the captured output tail, truncated to
    /// `TASK_OUTPUT_LIMIT` characters so a batch of running tasks cannot
    /// produce an unbounded payload.
    pub output: String,
    /// `display_meta.background` (delegate tasks); `false` otherwise.
    pub background: bool,
    /// `display_meta.workspace` (delegate tasks); `None` otherwise.
    pub workspace: Option<String>,
    /// The delegate subagent's session id (delegate tasks); `None` otherwise.
    /// Lets the web task panel jump straight to the subagent's transcript
    /// without label matching (labels vanish once the `running_tasks` row is
    /// cleared at task completion).
    pub subagent_session_id: Option<String>,
    /// The resumed subagent session id (delegate tasks); `None` otherwise.
    pub resume: Option<String>,
    /// The session that started this task; `None`/unknown falls back to
    /// `session_id` (the registry owner). For a subagent's bash task this
    /// is the subagent's own session id — the task panel shows the true
    /// initiator instead of the parent session.
    pub owner_session: Option<String>,
}

/// Map one [`BackgroundTaskInfo`] snapshot to its wire DTO. Pure function so
/// the field mapping (lossy UTF-8, truncation, display-meta flattening) is
/// unit-testable without a live task.
fn task_meta(session_id: &str, info: BackgroundTaskInfo) -> TaskMeta {
    let output = crate::session_store::task_output_preview(&String::from_utf8_lossy(&info.output));
    TaskMeta {
        session_id: session_id.to_owned(),
        id: info.id,
        label: info.label,
        full_command: info.full_command,
        role: info.role,
        kind: info.kind,
        output,
        background: info
            .display_meta
            .as_ref()
            .map(|meta| meta.background)
            .unwrap_or(false),
        subagent_session_id: info
            .display_meta
            .as_ref()
            .and_then(|meta| meta.subagent_session_id.clone()),
        resume: info
            .display_meta
            .as_ref()
            .and_then(|meta| meta.resume.clone()),
        workspace: info.display_meta.and_then(|meta| meta.workspace),
        // 发起者：live registry 记了 owner_session（subagent 的 bash）就透出，
        // None（主会话任务 / 未知）回退 session_id——前端用 owner_session 显示
        // 发起者 title，主会话任务与旧行为一致（session_id 查 title）。
        owner_session: info.owner_session.or_else(|| Some(session_id.to_owned())),
    }
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Handlers
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

/// Merge active registry sessions with historical metadata-table sessions
/// into one list. Registry entries win for ids present in both (they carry
/// live status); historical entries are marked `active: false` and
/// resumable. Pure function so the merge is unit-testable without a
/// backend.
fn merge_session_metas(
    active: Vec<SessionMeta>,
    historical: Vec<crate::session_store::SessionMeta>,
) -> Vec<SessionMeta> {
    let mut by_id: std::collections::HashMap<String, SessionMeta> =
        std::collections::HashMap::with_capacity(active.len() + historical.len());
    for mut meta in active {
        meta.active = true;
        by_id.insert(meta.id.clone(), meta);
    }
    for meta in historical {
        let id = meta.session_id.clone();
        match by_id.entry(id) {
            // A live session wins the list entry, but its title lives in
            // the metadata table (the registry DTO has no title), so
            // backfill it from the historical snapshot.
            std::collections::hash_map::Entry::Occupied(mut occupied) => {
                if occupied.get().title.is_none() {
                    occupied.get_mut().title = meta.title.clone();
                }
                // Same for the pin flag: the registry DTO has no pin, so
                // a live session's pinned state comes from its
                // metadata-table snapshot.
                if occupied.get().pinned.is_none() {
                    occupied.get_mut().pinned = meta.pinned;
                }
                // Same for the archive flag: the registry DTO has none, so
                // a live session's archived state comes from its
                // metadata-table snapshot.
                if occupied.get().archived.is_none() {
                    occupied.get_mut().archived = meta.archived;
                }
                // Same for the parent link: a resumed subagent's live entry
                // must stay recognizable as a subagent so `list_sessions`
                // can fill its label from `running_tasks`.
                if occupied.get().parent_session_id.is_none() {
                    occupied.get_mut().parent_session_id = meta.parent_session_id.clone();
                }
            }
            std::collections::hash_map::Entry::Vacant(vacant) => {
                vacant.insert(SessionMeta {
                    id: meta.session_id,
                    model: meta.model.unwrap_or_default(),
                    role: meta.role,
                    created_at: chrono::DateTime::from_naive_utc_and_offset(
                        meta.created_at,
                        chrono::Utc,
                    ),
                    last_active_at: chrono::DateTime::from_naive_utc_and_offset(
                        meta.last_active_at,
                        chrono::Utc,
                    ),
                    // Historical sessions are not running; "Idle" renders as the
                    // resumable chip and matches what a fresh resume shows.
                    status: "Idle".to_owned(),
                    entry_count: meta.entry_count.max(0) as usize,
                    busy: false,
                    active: false,
                    parent_session_id: meta.parent_session_id,
                    title: meta.title,
                    pinned: meta.pinned,
                    archived: meta.archived,
                    // Label lives in running_tasks; `list_sessions` fills it.
                    label: None,
                });
            }
        }
    }
    let mut metas: Vec<SessionMeta> = by_id.into_values().collect();
    // Backend sort (the frontend renders the array order as-is): pinned
    // sessions first (`pinned = Some(true)`), then unarchived sessions
    // before archived ones (`archived = Some(true)` sorts last), then
    // newest activity first within each group. Note the two flag
    // comparators run in OPPOSITE directions: pinned sorts descending
    // (true first), archived sorts ascending (true LAST — unarchived
    // sessions must stay in the default list).
    metas.sort_by(|a, b| {
        b.pinned
            .unwrap_or(false)
            .cmp(&a.pinned.unwrap_or(false))
            .then_with(|| {
                a.archived
                    .unwrap_or(false)
                    .cmp(&b.archived.unwrap_or(false))
            })
            .then_with(|| b.last_active_at.cmp(&a.last_active_at))
    });
    metas
}

/// Apply one subagent's running_tasks label lookup to its list entry.
/// Display-only: the label is task-panel metadata (`running_tasks.label`),
/// not runtime liveness — it has an async write/cleanup window, so a
/// surviving label alone never marks the entry live. `active`/`busy`/
/// `status` come exclusively from the main registry (`session_meta`) or
/// a live parent's `Sessions` registry (real handles), never from here.
/// Pure so the label rule is unit-testable without a Greptime backend.
fn apply_subagent_label(meta: &mut SessionMeta, label: Option<String>) {
    meta.label = label;
}

/// Threshold for logging slow `GET /api/sessions` requests: total handler
/// time at or above this (or any degraded error path) emits one
/// `eprintln!` line with per-phase durations and counts. Conservative so
/// normal requests stay silent. Never logs session ids/titles/paths.
const LIST_SESSIONS_SLOW_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(1);

/// Per-phase timings of one `list_sessions` call, in milliseconds.
#[derive(Default, Clone, Copy)]
struct ListSessionsTiming {
    live_ms: u128,
    list_meta_ms: u128,
    labels_ms: u128,
    child_scan_ms: u128,
    merge_ms: u128,
    connect_ms: u128,
    total_ms: u128,
    list_meta_diagnostics: Option<ListMetaDiagnostics>,
}

/// Build the slow-request log line, or `None` when the request was fast
/// and clean. Pure so the threshold/format is unit-testable.
#[allow(clippy::too_many_arguments)]
fn list_sessions_slow_log(
    timing: ListSessionsTiming,
    degraded: bool,
    live: usize,
    historical: usize,
    merged: usize,
    subagent_labels: usize,
) -> Option<String> {
    if !degraded && timing.total_ms < LIST_SESSIONS_SLOW_THRESHOLD.as_millis() {
        return None;
    }
    Some(format!(
        "e-agent: GET /api/sessions {}: total={}ms connect={}ms live={}ms list_meta={}ms labels={}ms child_scan={}ms merge={}ms counts(live={} historical={} merged={} subagent_labels={}){}",
        if degraded { "degraded" } else { "slow" },
        timing.total_ms,
        timing.connect_ms,
        timing.live_ms,
        timing.list_meta_ms,
        timing.labels_ms,
        timing.child_scan_ms,
        timing.merge_ms,
        live,
        historical,
        merged,
        subagent_labels,
        timing
            .list_meta_diagnostics
            .as_ref()
            .map(|d| format!(" {}", d.format_for_log()))
            .unwrap_or_default(),
    ))
}

async fn list_sessions(State(state): State<Arc<AppState>>) -> Json<Vec<SessionMeta>> {
    let request_start = std::time::Instant::now();
    let mut timing = ListSessionsTiming::default();
    let mut degraded = false;
    let root = state.factory.root();
    // Greptime reads deliberately use one ephemeral client for this request.
    // A failed ephemeral connect is degraded, never redirected to the shared
    // startup store (which would reintroduce its head-of-line queue).
    #[cfg(feature = "greptime")]
    let connect_start = std::time::Instant::now();
    #[cfg(feature = "greptime")]
    let mut request_read_store = None;
    #[cfg(feature = "greptime")]
    let mut request_read_failed = false;
    #[cfg(feature = "greptime")]
    if matches!(
        state.factory.backend(),
        crate::config::SessionBackend::Greptime { .. }
    ) {
        match SessionStore::connect_meta_read_only(state.factory.backend(), root).await {
            Ok(store) => request_read_store = Some(store),
            Err(error) => {
                eprintln!("e-agent: cannot open sessions read connection: {error:#}");
                request_read_failed = true;
            }
        }
        timing.connect_ms = connect_start.elapsed().as_millis();
    }
    let phase_start = std::time::Instant::now();
    let mut active: Vec<SessionMeta> = Vec::with_capacity(state.registry.list().len());
    // Main-registry ids: an entry restored/resumed into the main registry
    // already carries its real status/busy/active from `session_meta`, so
    // the child-handle backfill below must skip it — a stale same-id child
    // handle must never overwrite the authoritative registry state.
    let mut main_registry_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (id, session) in state.registry.list() {
        main_registry_ids.insert(id.clone());
        active.push(session_meta(&id, &session, root).await);
    }
    timing.live_ms = phase_start.elapsed().as_millis();
    let live_count = active.len();
    let phase_start = std::time::Instant::now();
    // Historical sessions from the metadata table (Greptime/SQLite audit
    // table; JSONL `.meta.jsonl` sidecars).
    let historical_result = {
        #[cfg(feature = "greptime")]
        if let Some(store) = request_read_store.as_ref() {
            store.list_meta_with_diagnostics(root).await
        } else if request_read_failed {
            Err(anyhow!(
                "ephemeral Greptime sessions read connection unavailable"
            ))
        } else {
            async {
                let store = meta_store(&state).await?;
                store.list_meta_with_diagnostics(root).await
            }
            .await
        }
        #[cfg(not(feature = "greptime"))]
        {
            async {
                let store = meta_store(&state).await?;
                store.list_meta_with_diagnostics(root).await
            }
            .await
        }
    };
    let historical = match historical_result {
        Ok((list, diagnostics)) => {
            timing.list_meta_diagnostics = Some(diagnostics);
            list
        }
        Err(error) => {
            eprintln!("e-agent: cannot list session metadata: {error:#}");
            degraded = true;
            Vec::new()
        }
    };
    timing.list_meta_ms = phase_start.elapsed().as_millis();
    let historical_count = historical.len();
    let phase_start = std::time::Instant::now();
    let mut merged = merge_session_metas(active, historical);
    // Subagent items carry the task-panel label of their delegate task,
    // which lives in `running_tasks` (the sessions metadata table has no
    // label column). JSONL resolves ALL labels in one directory scan (one
    // read_dir + one pass over the record files, on a blocking thread) —
    // the per-item loop was an N+1 that re-scanned the directory per
    // subagent. Greptime/SQLite have no batched query, so they keep the
    // per-item loop: each lookup is a cheap indexed query, not a file
    // scan.
    let mut labels: HashMap<String, Option<String>> = HashMap::new();
    let labels_result = {
        #[cfg(feature = "greptime")]
        if let Some(store) = request_read_store.as_ref() {
            store.all_subagent_labels(root).await
        } else if request_read_failed {
            Err(anyhow!(
                "ephemeral Greptime sessions read connection unavailable"
            ))
        } else {
            async {
                let store = meta_store(&state).await?;
                store.all_subagent_labels(root).await
            }
            .await
        }
        #[cfg(not(feature = "greptime"))]
        {
            async {
                let store = meta_store(&state).await?;
                store.all_subagent_labels(root).await
            }
            .await
        }
    };
    match labels_result {
        Ok(Some(all)) => labels = all,
        Ok(None) => {
            for meta in &merged {
                if meta.parent_session_id.is_some() {
                    match async {
                        let store = meta_store(&state).await?;
                        store.label_for_subagent(root, &meta.id).await
                    }
                    .await
                    {
                        Ok(label) => {
                            labels.insert(meta.id.clone(), label);
                        }
                        Err(error) => {
                            eprintln!("e-agent: cannot look up subagent label: {error:#}");
                            degraded = true;
                        }
                    }
                }
            }
        }
        Err(error) => {
            eprintln!("e-agent: cannot look up subagent labels: {error:#}");
            degraded = true;
        }
    }
    timing.labels_ms = phase_start.elapsed().as_millis();
    let subagent_label_count = labels.len();
    let phase_start = std::time::Instant::now();
    // Live subagent handles live in their parent session's `Sessions`
    // registry, not the main registry, so `session_meta` never sees their
    // real status. Snapshot them AFTER the async label queries so every
    // entry sees the same view: subagent session id → current handle
    // status. A subagent id can hold several handles (resume re-registers);
    // any Busy/Compacting handle wins — an Idle handle must not mask a
    // sibling that is still working.
    let mut subagent_status: std::collections::HashMap<String, SessionStatus> =
        std::collections::HashMap::new();
    for (_, session) in state.registry.list() {
        for (_, entry) in session.sessions.list() {
            let status = entry.handle.status();
            let status = status.borrow().clone();
            let running = matches!(status, SessionStatus::Busy | SessionStatus::Compacting);
            let overwrite = match subagent_status.get(&entry.session_id) {
                None => true,
                Some(existing) => {
                    !matches!(existing, SessionStatus::Busy | SessionStatus::Compacting)
                }
            };
            if running || overwrite {
                subagent_status.insert(entry.session_id.clone(), status);
            }
        }
    }
    timing.child_scan_ms = phase_start.elapsed().as_millis();
    let phase_start = std::time::Instant::now();
    for meta in &mut merged {
        if meta.parent_session_id.is_none() {
            continue;
        }
        // A real handle in a live parent's `Sessions` registry means the
        // subagent is live right now: backfill its real status/busy and
        // mark it active. Entries with no handle keep their merged values
        // untouched, and a stale label alone never marks an entry live
        // (test 5 in `list_sessions_subagent_liveness` covers exactly
        // that). A main-registry entry is skipped entirely: it already has
        // the authoritative status/busy/active from `session_meta`, and a
        // stale same-id child handle must not overwrite it (test 7 in
        // `list_sessions_subagent_liveness_matrix` covers that).
        if !main_registry_ids.contains(&meta.id)
            && let Some(status) = subagent_status.get(&meta.id)
        {
            meta.status = status_string(status).to_owned();
            meta.busy = matches!(status, SessionStatus::Busy | SessionStatus::Compacting);
            meta.active = true;
        }
        if let Some(label) = labels.get(&meta.id) {
            apply_subagent_label(meta, label.clone());
        }
    }
    timing.merge_ms = phase_start.elapsed().as_millis();
    timing.total_ms = request_start.elapsed().as_millis();
    if let Some(message) = list_sessions_slow_log(
        timing,
        degraded,
        live_count,
        historical_count,
        merged.len(),
        subagent_label_count,
    ) {
        eprintln!("{message}");
    }
    Json(merged)
}

#[derive(Deserialize)]
struct CreateSessionBody {
    /// Optional caller-chosen id; defaults to a fresh `web-…` id.
    #[serde(default)]
    id: Option<String>,
    /// Optional first prompt; queued on the runner before it starts so the
    /// session begins a turn immediately (empty/whitespace = no prompt).
    #[serde(default)]
    initial_prompt: Option<String>,
}

async fn create_session(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateSessionBody>,
) -> Result<(StatusCode, Json<SessionMeta>), (StatusCode, String)> {
    // The resume guard below applies only to an EXPLICIT id (a resume). A
    // generated `web-…` id is fresh by construction and cannot collide with
    // a running subagent, so it skips the cross-process lookup. Captured
    // BEFORE the match moves `body.id` out.
    let explicit_id = body.id.is_some();
    let id = match body.id {
        Some(id) => {
            crate::session::validate_session_name(&id)
                .map_err(|e| error(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            id
        }
        None => crate::session::new_id_prefixed("web-"),
    };
    let reservation = match state.registry.reserve(&id) {
        Ok(reservation) => reservation,
        Err(live) => {
            return Err(error(
                StatusCode::CONFLICT,
                if live {
                    format!("session {id} already exists")
                } else {
                    format!("session {id} is currently being created")
                },
            ));
        }
    };
    // Resume guard (concurrent-write conflicts #46/#49): resuming a
    // subagent that is still running would build a SECOND runner over the
    // same session file — both would append, and Greptime/SQLite reject
    // the out-of-order seq ("database max seq is N but this writer
    // expected …"). The owning session knows its live subagents, so block
    // any id that is currently a live handle in some parent's `Sessions`
    // registry. A handle exists only while the subagent is alive
    // (DelegateCleanup removes finished ones), so a hit here always means
    // "still running" — an Idle btw subagent counts too, it still owns
    // the file. Finished subagents resume normally.
    if let Some((parent, task_id)) = subagent_is_live(&state, &id) {
        return Err(error(
            StatusCode::CONFLICT,
            format!(
                "session {id} is a running subagent (spawned by session {parent}, task {task_id}); \
                 cannot resume it while it runs — wait for it to finish or cancel the task first"
            ),
        ));
    }
    // Cross-process backstop: a surviving `running_tasks` row for this id
    // means a delegate task in ANOTHER process still claims the session
    // (rows are cleared at task completion, so a surviving row = "not
    // finished yet", or a zombie row left by a dead process). Resuming
    // would race that owner's writes, so block conservatively — the row is
    // the only cross-process liveness signal. Zombie rows self-heal:
    // resuming the owning parent session from the CLI/TUI
    // (UnfinishedPolicy::Consume) or a delegate `resume` consumes them.
    let root = state.factory.root();
    if explicit_id
        && meta_store(&state)
            .await
            .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
            .label_for_subagent(root, &id)
            .await
            .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
            .is_some()
    {
        return Err(error(
            StatusCode::CONFLICT,
            format!(
                "session {id} is still claimed by a running delegate task (possibly in another \
                 e-agent process); cannot resume it while it runs — wait for it to finish, or if \
                 the owning process died, resume its parent session first"
            ),
        ));
    }
    // `build_session` uses the factory's current model (which is correct for
    // execution) and idempotently creates metadata if necessary. Read an
    // existing session's metadata first so a resumed session keeps its
    // original model in the web-facing live registry.
    let persisted_model = if explicit_id {
        meta_store(&state)
            .await
            .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
            .list_meta(root)
            .await
            .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
            .into_iter()
            .find(|meta| meta.session_id == id)
            .and_then(|meta| meta.model)
    } else {
        None
    };
    let built = build_session(&state.factory, &id).await?;
    let initial_prompt = body.initial_prompt.filter(|p| !p.trim().is_empty());
    let session = Arc::new(LiveSession {
        handle: built.handle,
        task: built.runner.start(initial_prompt),
        store: built.store,
        background: built.background,
        sessions: built.sessions,
        model_name: Mutex::new(persisted_model.unwrap_or(built.model_name)),
        role_name: built.role_name,
        created_at: chrono::Utc::now(),
    });
    reservation.publish(session.clone());
    // 桌宠总结：每个 turn（Busy→Idle）结束时后台生成一句话中文总结并缓存。
    // 监听任务订阅 runner 的 status watch（不改 runner.rs）；会话删除/运行器
    // 退出（watch sender drop）时自动结束。
    spawn_summary_listener(state.clone(), id.clone(), session.clone());
    Ok((
        StatusCode::CREATED,
        Json(session_meta(&id, &session, root).await),
    ))
}

/// One session build from the shared factory. Web sessions are interactive:
/// they wait for input rather than finishing when idle.
async fn build_session(
    factory: &SessionFactory,
    id: &str,
) -> Result<SessionBuild, (StatusCode, String)> {
    // Lazy server attach: the session may still be live in another process
    // (the server restarts far more often than its sessions die), so the
    // unfinished-background records are NOT blindly consumed. Probe whether
    // every record was left by a now-dead process: only then is it safe to
    // use Consume — take the records and inject the "killed with the
    // process" notice, exactly like a TUI/CLI restart. Any uncertainty
    // (a live owner, an old record without an owner, a probe failure)
    // keeps Preserve: the owning process may still be alive and clears its
    // own records via ack_background_entry → clear_background_task. No
    // records → Consume is a harmless no-op, so the probe reports true.
    let unfinished = {
        let root = factory.root();
        // One throwaway store for the probe (build() connects its own);
        // JSONL is a zero-cost marker, Greptime/SQLite just open a second
        // short-lived connection bound to the same session id.
        let store = SessionStore::connect(factory.backend(), root, id)
            .await
            .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
        match store.unfinished_owner_all_dead(root, id).await {
            // Every owner dead (or no records): safe to consume and
            // inject the "killed with the process" notice.
            Ok(true) => UnfinishedPolicy::Consume,
            // Some owner is alive or unjudgeable: leave the records for
            // the owning process.
            Ok(false) => UnfinishedPolicy::Preserve,
            // Probe failure must not take the whole session build down —
            // the session itself is perfectly usable. Degrade to the
            // conservative Preserve (matching the migration-failure
            // degradation style elsewhere: eprintln + keep running).
            Err(e) => {
                eprintln!(
                    "e-agent: cannot probe unfinished background-task owners, \
                     keeping Preserve: {e:#}"
                );
                UnfinishedPolicy::Preserve
            }
        }
    };
    factory
        .build(id, None, None, IdlePolicy::WaitForInput, unfinished)
        .await
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))
}

/// Resolve a session id to something the web can attach to: the main
/// registry first, then every live session's subagent registry (a subagent
/// is addressable by its session id exactly like the TUI attaches to it).
/// The scan is bounded in practice — a handful of live sessions, each with
/// a handful of subagents — and only runs on a registry miss.
fn live(state: &AppState, id: &str) -> Result<SessionRef, (StatusCode, String)> {
    if let Some(session) = state.registry.get(id) {
        return Ok(SessionRef::Live(session));
    }
    for (_session_id, session) in state.registry.list() {
        for (task_id, entry) in session.sessions.list() {
            if entry.session_id == id {
                return Ok(SessionRef::Subagent {
                    entry,
                    parent: session,
                    task_id,
                });
            }
        }
    }
    Err(error(
        StatusCode::NOT_FOUND,
        format!("session {id} not found"),
    ))
}

/// True when `id` is a live subagent handle in some live session's
/// `Sessions` registry — the in-process half of the resume guard
/// ([`create_session`]). A handle only exists while the subagent is alive
/// (finished delegates are removed by `DelegateCleanup`), so a hit here
/// always means "still running": the subagent's runner owns the session
/// file and a second runner would write it concurrently (#46/#49). An Idle
/// btw subagent is also still alive and must block resume for the same
/// reason. Returns the owning parent session id and task id so the caller
/// can name them in the rejection. Same bounded scan as [`live`].
fn subagent_is_live(state: &AppState, id: &str) -> Option<(String, u64)> {
    for (session_id, session) in state.registry.list() {
        for (task_id, entry) in session.sessions.list() {
            if entry.session_id == id {
                return Some((session_id.clone(), task_id));
            }
        }
    }
    None
}

#[derive(Deserialize)]
struct PromptBody {
    /// The frontend sends `JSON.stringify({ text })`; accept both `text`
    /// and `prompt` so the endpoint is agnostic to the client's field name.
    #[serde(alias = "text")]
    prompt: String,
}

async fn session_prompt(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<PromptBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    if body.prompt.trim().is_empty() {
        return Err(error(StatusCode::BAD_REQUEST, "prompt must not be empty"));
    }
    let session = live(&state, &id)?;
    let handle = session.handle();
    let status = handle.status();
    if matches!(*status.borrow(), SessionStatus::Finished(_)) {
        return Err(error(
            StatusCode::CONFLICT,
            format!("session {id} has finished"),
        ));
    }
    handle.prompt(body.prompt);
    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize)]
struct BtwBody {
    /// The question; becomes the btw subagent's first user message.
    prompt: String,
}

/// `POST /api/sessions/{id}/btw` — fork the live session's full history
/// into a persistent interactive subagent ("btw fork") and start it
/// immediately with the question as its first user message. The subagent
/// runs under `IdlePolicy::WaitForInput` — unlike a `delegate` task it
/// never completes on its own, so it stays alive for further turns until
/// the user closes it (task-panel cancel / process exit) — and is
/// registered in the task panel + sessions metadata, so it shows up in the
/// sidebar and can be attached to exactly like any other subagent.
/// 201 + `{"id": "<btw-…>"}`; 404 when the source session is not live.
async fn session_btw(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<BtwBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)> {
    if body.prompt.trim().is_empty() {
        return Err(error(StatusCode::BAD_REQUEST, "prompt must not be empty"));
    }
    let session = live(&state, &id)?;
    let SessionRef::Live(session) = session else {
        // A subagent does not own a `BackgroundTasks`/`Sessions` registry to
        // fork from (those belong to its parent); btw forking stays a
        // main-session feature.
        return Err(error(
            StatusCode::CONFLICT,
            format!("cannot fork subagent session {id}"),
        ));
    };
    let subagent_id = crate::delegate::spawn_btw_subagent(
        &id,
        &body.prompt,
        crate::delegate::BtwContext {
            // The btw subagent runs on the source session's own model
            // (user-confirmed): the factory's main model, shared by every
            // built session.
            model: state.factory.main_model().clone(),
            context_window: state.factory.main_context_window(),
            workspace: state.factory.workspace().clone(),
            sandbox: state.factory.sandbox().cloned(),
            read_only: state.factory.read_only(),
            background: session.background.clone(),
            sessions: session.sessions.clone(),
            persist_root: state.factory.root().to_path_buf(),
            backend: state.factory.backend().clone(),
            record_in: Some(crate::session_store::BackgroundRecord {
                root: state.factory.root().to_path_buf(),
                session: id.clone(),
                store: session.store.clone(),
            }),
        },
    )
    .await
    .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "id": subagent_id })),
    ))
}

async fn session_cancel(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let session = live(&state, &id)?;
    session.handle().cancel();
    Ok(StatusCode::ACCEPTED)
}

async fn session_compact(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let session = live(&state, &id)?;
    session.handle().compact();
    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize)]
struct ModelBody {
    profile: String,
}

/// `GET /api/models` — every switchable model profile name (`[models]` keys
/// plus `[roles]` values from the same config the factory was built with),
/// sorted, for the web `/model <profile>` autocomplete. JSON array of
/// strings; `[]` when there is no config.
async fn list_models(State(state): State<Arc<AppState>>) -> Json<Vec<String>> {
    Json(state.factory.model_profiles())
}

#[derive(Debug, Serialize)]
struct PetRuntimeConfig {
    enabled: bool,
    cols: u32,
    rows: u32,
    frame_width: u32,
    frame_height: u32,
    idle_row: u32,
    idle_frames: u32,
    loop_ms: u64,
}

/// `GET /api/pet/config` — live desktop pet settings. This reads the
/// reloadable config on every request, so saving the config switches sprite
/// mode without restarting the server.
async fn pet_config(State(state): State<Arc<AppState>>) -> Json<PetRuntimeConfig> {
    use crate::config::{
        DEFAULT_PET_FRAME_HEIGHT, DEFAULT_PET_FRAME_WIDTH, DEFAULT_PET_IDLE_FRAMES,
        DEFAULT_PET_IDLE_ROW, DEFAULT_PET_LOOP_MS, DEFAULT_PET_SPRITE_COLS,
        DEFAULT_PET_SPRITE_ROWS,
    };

    let pet = state
        .factory
        .current_config()
        .and_then(|config| config.pet().cloned());
    let cols = pet
        .as_ref()
        .and_then(|pet| pet.sprite_cols)
        .unwrap_or(DEFAULT_PET_SPRITE_COLS)
        .max(1);
    let rows = pet
        .as_ref()
        .and_then(|pet| pet.sprite_rows)
        .unwrap_or(DEFAULT_PET_SPRITE_ROWS)
        .max(1);
    Json(PetRuntimeConfig {
        enabled: pet
            .as_ref()
            .and_then(|pet| pet.spritesheet.as_ref())
            .is_some(),
        cols,
        rows,
        frame_width: pet
            .as_ref()
            .and_then(|pet| pet.frame_width)
            .unwrap_or(DEFAULT_PET_FRAME_WIDTH)
            .max(1),
        frame_height: pet
            .as_ref()
            .and_then(|pet| pet.frame_height)
            .unwrap_or(DEFAULT_PET_FRAME_HEIGHT)
            .max(1),
        idle_row: pet
            .as_ref()
            .and_then(|pet| pet.idle_row)
            .unwrap_or(DEFAULT_PET_IDLE_ROW)
            .min(rows - 1),
        idle_frames: pet
            .as_ref()
            .and_then(|pet| pet.idle_frames)
            .unwrap_or(DEFAULT_PET_IDLE_FRAMES)
            .clamp(1, cols),
        loop_ms: pet
            .as_ref()
            .and_then(|pet| pet.loop_ms)
            .unwrap_or(DEFAULT_PET_LOOP_MS)
            .max(1),
    })
}

/// `POST /api/sessions/{id}/model` — switch the session's model at runtime
/// (web `/model <profile>`). Body `{"profile": "provider/model"}`; the
/// profile is resolved against the same config the factory was built with
/// (honoring `--base-url`/`--model` overrides), then installed on the live
/// runner's agent — the session keeps its history and continues with the
/// new model. 200 + `{"ok": true, "model": "<profile key>"}` on success;
/// 400 for an unknown profile / no config; 404 for an unknown session.
async fn session_model(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<ModelBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let session = live(&state, &id)?;
    let profile = body.profile.trim();
    if profile.is_empty() {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "model profile must be provider/model (e.g. chatgpt/sol)",
        ));
    }
    let configured = state.factory.resolve_profile(profile).map_err(|e| {
        error(
            StatusCode::BAD_REQUEST,
            format!("unknown model profile `{profile}`: {e:#}"),
        )
    })?;
    let name = configured.profile_key();
    session.handle().switch_model(Box::new(configured));
    // Mirror the new model into the registry metadata so `GET /api/sessions`
    // (sidebar / composer meta) reflects the switch immediately. Subagent
    // sessions keep their spawn-time display name (the delegate entry is
    // immutable); the switch itself still applies to their runner.
    if let SessionRef::Live(session) = &session {
        session.set_model_name(name.clone());
    }
    Ok(Json(serde_json::json!({ "ok": true, "model": name })))
}

/// `POST /api/sessions/{id}/undo` — undo the most recent file operation
/// (`edit_file` / `write_file`) on the workspace it ran on. The undo stack
/// is process-global and session-agnostic; the endpoint lives under the
/// session path only to reuse authentication/routing. 200 + `{"ok": true,
/// "message": "已撤销 write_file: path"}` on success; 404 for an unknown
/// session id; 409 with a Chinese error message when there is nothing to
/// undo or the file has been modified since (never a panic).
async fn session_undo(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    live(&state, &id)?;
    match crate::tools::undo_file_op() {
        Ok(message) => Ok(Json(serde_json::json!({ "ok": true, "message": message }))),
        Err(message) => Err(error(StatusCode::CONFLICT, message)),
    }
}

/// `GET /api/sessions/{id}/goal` — the committed goal snapshot (`null`
/// when none is set / cleared). 200; 404 for an unknown session.
async fn session_goal_get(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let session = live(&state, &id)?;
    Ok(Json(serde_json::json!({ "goal": session.handle().goal() })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalBody {
    /// "set" (create, human-only) | "pause" | "resume" | "clear".
    action: String,
    /// Required for `set`.
    objective: Option<String>,
    success_criteria: Option<Vec<String>>,
}

/// `POST /api/sessions/{id}/goal` — human goal mutation, applied by the
/// runner (which re-validates atomically and persists a `GoalUpdated`
/// entry; results/errors fan out over SSE). 202 Accepted (async, like
/// prompt/compact); 400/409 for input the runner would reject, 409 when
/// the session is finished or its command channel is closed (the mutation
/// could never apply); 404 for an unknown session.
async fn session_goal_post(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<GoalBody>, JsonRejection>,
) -> Result<StatusCode, (StatusCode, String)> {
    // Strict body contract: `deny_unknown_fields` makes misspelled/extra
    // fields a deserialization failure, mapped to a plain 400 (axum's
    // default Json rejection would be 422). The handler never runs, so a
    // rejected body is never queued or applied.
    let Json(body) = body.map_err(|rejection| {
        error(
            StatusCode::BAD_REQUEST,
            format!("invalid goal body: {rejection}"),
        )
    })?;
    let session = live(&state, &id)?;
    let handle = session.handle();
    // A finished/closed session can never apply a goal mutation: reject
    // synchronously instead of a hollow 202 that the runner would drop.
    if matches!(&*handle.status().borrow(), SessionStatus::Finished(_)) {
        return Err(error(
            StatusCode::CONFLICT,
            "session is finished: cannot modify its goal",
        ));
    }
    let command = match body.action.as_str() {
        "set" => {
            let objective = body.objective.unwrap_or_default();
            if objective.trim().is_empty() {
                return Err(error(
                    StatusCode::BAD_REQUEST,
                    "goal objective must not be empty",
                ));
            }
            // Pre-validate the create rule synchronously; the runner
            // re-checks under the same rule (a race surfaces as an SSE
            // `error` event).
            if let Some(goal) = handle.goal()
                && goal.status != crate::agent::GoalStatus::Completed
            {
                return Err(error(
                    StatusCode::CONFLICT,
                    format!(
                        "session already has a goal (`{}`, status {}): complete or clear it first",
                        goal.id,
                        goal.status.label()
                    ),
                ));
            }
            crate::runner::GoalCommand::Create {
                objective,
                success_criteria: body.success_criteria.unwrap_or_default(),
            }
        }
        "pause" | "resume" | "clear" => {
            if handle.goal().is_none() {
                return Err(error(
                    StatusCode::CONFLICT,
                    "no goal is set for this session",
                ));
            }
            let action = match body.action.as_str() {
                "pause" => crate::agent::GoalAction::Pause,
                "resume" => crate::agent::GoalAction::Resume,
                _ => crate::agent::GoalAction::Clear,
            };
            crate::runner::GoalCommand::Action(action)
        }
        other => {
            return Err(error(
                StatusCode::BAD_REQUEST,
                format!("unknown goal action `{other}` (known: set, pause, resume, clear)"),
            ));
        }
    };
    // Never a fake 202: if the command could not be queued (channel closed
    // between the status check and the send), report the conflict.
    if !handle.goal_command(command) {
        return Err(error(
            StatusCode::CONFLICT,
            "session is finished or its command channel is closed: goal not accepted",
        ));
    }
    Ok(StatusCode::ACCEPTED)
}

/// Cap for user-assigned session titles: longer titles are truncated
/// (chars, so multi-byte text is preserved), never rejected with 400 —
/// the simplest contract, per the manual-naming design.
const TITLE_MAX_CHARS: usize = 200;

#[derive(Deserialize)]
struct TitleBody {
    /// The frontend sends `JSON.stringify({ title })`.
    title: String,
}

/// `PUT /api/sessions/{id}/title` — rename a session. Body
/// `{"title": "..."}`; empty/whitespace clears the title (stored NULL),
/// longer titles are truncated to [`TITLE_MAX_CHARS`]. Works for live
/// sessions and live subagents (via their session-bound store — a built
/// session always has a metadata row, `build` → `create_meta`) and
/// historical sessions (via the workspace-scoped `meta_store`, so a rename
/// survives the session leaving the registry). 404 when the id is neither
/// live nor present in the metadata table.
async fn session_title(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<TitleBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    let trimmed = body.title.trim();
    let title = if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(TITLE_MAX_CHARS).collect::<String>())
    };
    let root = state.factory.root();
    let store = match live(&state, &id) {
        Ok(SessionRef::Live(session)) => session.store.clone(),
        // A live subagent's own store is bound to its session id, so the
        // rename lands on the same rows the meta_store fallback would use.
        Ok(SessionRef::Subagent { entry, .. }) => entry.store.clone(),
        Err(_) => {
            let historical = {
                let store = meta_store(&state)
                    .await
                    .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
                store
                    .list_meta(root)
                    .await
                    .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
            };
            if !historical.iter().any(|m| m.session_id == id) {
                return Err(error(
                    StatusCode::NOT_FOUND,
                    format!("session {id} not found"),
                ));
            }
            meta_store(&state)
                .await
                .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
        }
    };
    store
        .set_title(root, &id, title.as_deref())
        .await
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct PinBody {
    /// The frontend sends `JSON.stringify({ pinned })`.
    pinned: bool,
}

/// `PUT /api/sessions/{id}/pin` — pin or unpin a session. Body
/// `{"pinned": true|false}`; one endpoint toggles both directions. Works
/// for live sessions and live subagents (via their session-bound store —
/// a built session always has a metadata row, `build` → `create_meta`) and
/// historical sessions (via the workspace-scoped `meta_store`, so a pin
/// survives the session leaving the registry). 404 when the id is neither
/// live nor present in the metadata table.
async fn session_pin(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<PinBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    let root = state.factory.root();
    let store = match live(&state, &id) {
        Ok(SessionRef::Live(session)) => session.store.clone(),
        Ok(SessionRef::Subagent { entry, .. }) => entry.store.clone(),
        Err(_) => {
            let historical = {
                let store = meta_store(&state)
                    .await
                    .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
                store
                    .list_meta(root)
                    .await
                    .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
            };
            if !historical.iter().any(|m| m.session_id == id) {
                return Err(error(
                    StatusCode::NOT_FOUND,
                    format!("session {id} not found"),
                ));
            }
            meta_store(&state)
                .await
                .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
        }
    };
    store
        .set_pinned(root, &id, body.pinned)
        .await
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ArchiveBody {
    /// The frontend sends `JSON.stringify({ archived })`.
    archived: bool,
}

/// `PUT /api/sessions/{id}/archive` — archive or restore a session. Body
/// `{"archived": true|false}`; one endpoint toggles both directions. Works
/// for live sessions and live subagents (via their session-bound store —
/// a built session always has a metadata row, `build` → `create_meta`) and
/// historical sessions (via the workspace-scoped `meta_store`, so an
/// archive survives the session leaving the registry). 404 when the id is
/// neither live nor present in the metadata table.
async fn session_archive(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<ArchiveBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    let root = state.factory.root();
    let store = match live(&state, &id) {
        Ok(SessionRef::Live(session)) => session.store.clone(),
        Ok(SessionRef::Subagent { entry, .. }) => entry.store.clone(),
        Err(_) => {
            let historical = {
                let store = meta_store(&state)
                    .await
                    .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
                store
                    .list_meta(root)
                    .await
                    .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
            };
            if !historical.iter().any(|m| m.session_id == id) {
                return Err(error(
                    StatusCode::NOT_FOUND,
                    format!("session {id} not found"),
                ));
            }
            meta_store(&state)
                .await
                .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
        }
    };
    store
        .set_archived(root, &id, body.archived)
        .await
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    // Known limitation (pre-existing, unrelated to the steering rework):
    // for a WaitForInput web session this can leave an idle runner behind.
    // `handle().cancel()` is a *release* that parks a WaitForInput session
    // at Idle, and any other holder of the `Arc<LiveSession>` (e.g. an
    // in-flight request) keeps the registry entry's `SessionTask` alive
    // past `registry.remove` — an unaddressable idle runner that only ends
    // when the last Arc drops or the process exits. Fixing this is tracked
    // separately; the transcript stays and a later resume still works.
    //
    // Live session: cancel + remove from the registry. Dropping the
    // registry entry (and with it the SessionTask) aborts the runner;
    // in-flight SSE streams notice the closed event/status channels and
    // end themselves.
    //
    // A live subagent is truly terminated through its parent's
    // `BackgroundTasks::cancel(task_id)` instead of a plain
    // `handle().cancel()`: the latter is a *release* — it preempts the
    // in-flight operation and (for a FinishWhenIdle subagent with nothing
    // queued) finalizes the runner as Cancelled, but it does not remove the
    // `Sessions` registry entry / running_tasks row or abort a runner that
    // is parked at an idle select. The abort drops the delegate wrapper,
    // whose captured cleanup removes the `Sessions` entry and the
    // running_tasks row, and dropping the wrapper's runner handle aborts
    // the subagent runner (SessionTask::drop). This is the same path
    // the task-panel cancel (`DELETE /api/sessions/{parent}/tasks/{id}`)
    // uses. The transcript stays in its session file either way.
    if let Ok(session) = live(&state, &id) {
        match session {
            SessionRef::Live(session) => {
                session.handle.cancel();
                state.registry.remove(&id);
            }
            SessionRef::Subagent {
                parent, task_id, ..
            } => {
                parent.background.cancel(task_id);
            }
        }
    }
    // Hide from the sessions list: delete the session's metadata rows
    // (Greptime/SQLite audit table; JSONL sidecar file). The transcript
    // stays, so a later resume still works.
    let root = state.factory.root();
    meta_store(&state)
        .await
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
        .delete_meta(root, &id)
        .await
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(StatusCode::NO_CONTENT)
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Background tasks
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

/// `GET /api/tasks` — one flat snapshot of every running background task
/// across all live sessions (bash and delegate). Task ids are only unique
/// per session, so each entry carries its `session_id`; the list is sorted
/// by `(session_id, id)` for a stable render across registry iteration
/// order (the registry is a HashMap). Delegate entries additionally carry
/// `subagent_session_id` so clients can jump straight to the subagent's
/// transcript without label matching.
async fn list_tasks(State(state): State<Arc<AppState>>) -> Json<Vec<TaskMeta>> {
    let mut tasks: Vec<TaskMeta> = Vec::new();
    let root = state.factory.root();
    for (session_id, session) in state.registry.list() {
        for info in session.background.running() {
            let mut meta = task_meta(&session_id, info);
            // live registry 缺 full_command 时（delegate 任务、旧版本启动的
            // 任务、注册瞬间等），从 running_tasks 的 full_command 列回退
            // 补上——数据持久化在 DB，重启后 UI 仍能显示完整命令。查不到
            // （行已消费/该字段缺失/NULL）保持 None，与旧行为一致。
            if meta.full_command.is_none() {
                match async {
                    let store = meta_store(&state).await?;
                    store.task_full_command(root, &session_id, meta.id).await
                }
                .await
                {
                    Ok(Some(command)) => meta.full_command = Some(command),
                    Ok(None) => {}
                    Err(error) => {
                        eprintln!(
                            "e-agent: cannot look up background task full command \
                             for session {session_id} task {}: {error:#}",
                            meta.id
                        );
                    }
                }
            }
            tasks.push(meta);
        }
    }
    tasks.sort_by(|a, b| (&a.session_id, a.id).cmp(&(&b.session_id, b.id)));
    Json(tasks)
}

/// `GET /api/tasks/finished` — the most recent finished background tasks
/// across all sessions, newest first, read from the durable
/// `session_entries` rows (`entry_kind='background_completion'`) — the
/// ONLY authoritative record, exactly like the live `/api/tasks` snapshot
/// stays registry-backed. Each entry carries its trace metadata
/// (label/kind/status/exit_code/signal/duration/started_at) plus its
/// session id and seq (the seq relates it to the session's other entries
/// for turn reconstruction; `finished_at` is the row's event_time).
/// Sorted newest-first with a sane limit (100). JSONL workspaces return an
/// empty list (no `session_entries` table — same limitation as
/// `usage_summary`).
const FINISHED_TASKS_LIMIT: usize = 100;

async fn finished_tasks(State(state): State<Arc<AppState>>) -> Json<Vec<FinishedTask>> {
    let root = state.factory.root();
    match async {
        let store = meta_store(&state).await?;
        store.finished_tasks(root, FINISHED_TASKS_LIMIT).await
    }
    .await
    {
        Ok(tasks) => Json(tasks),
        Err(error) => {
            eprintln!("e-agent: cannot query finished background tasks: {error:#}");
            Json(Vec::new())
        }
    }
}
/// `DELETE /api/sessions/{id}/tasks/{task_id}` — cancel one running
/// background task. 204 when cancelled; 404 for an unknown session or an
/// unknown task id in that session.
async fn cancel_task(
    State(state): State<Arc<AppState>>,
    Path((id, task_id)): Path<(String, u64)>,
) -> Result<StatusCode, (StatusCode, String)> {
    let session = live(&state, &id)?;
    let SessionRef::Live(session) = session else {
        // A subagent's background tasks live in its parent's registry and
        // are addressed by the parent's session id; a subagent id has no
        // task registry of its own.
        return Err(error(
            StatusCode::NOT_FOUND,
            format!("session {id} has no background task registry"),
        ));
    };
    match session.background.cancel(task_id) {
        Some(_) => Ok(StatusCode::NO_CONTENT),
        None => Err(error(
            StatusCode::NOT_FOUND,
            format!("task {task_id} not found in session {id}"),
        )),
    }
}

/// `GET /api/sessions/{id}/tasks/{task_id}/output` — the full captured
/// output of one running background bash task as `text/plain` (lossy
/// UTF-8). Unlike `/api/tasks`' 2000-char output tail this is the
/// untruncated full spool (keep-first capped at 16 MiB), so the frontend
/// can poll it while a task streams. `Cache-Control: no-cache` keeps
/// polling honest (the global API middleware upgrades it to `no-store` on
/// the wire). 404 for an unknown session, a subagent session (its
/// tasks live in the parent's registry), or a task with no output spool
/// (delegate tasks).
async fn task_output(
    State(state): State<Arc<AppState>>,
    Path((id, task_id)): Path<(String, u64)>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let session = live(&state, &id)?;
    let SessionRef::Live(session) = session else {
        // A subagent's background tasks live in its parent's registry and
        // are addressed by the parent's session id; a subagent id has no
        // task registry of its own.
        return Err(error(
            StatusCode::NOT_FOUND,
            format!("session {id} has no background task registry"),
        ));
    };
    match session.background.output(task_id) {
        Some(output) => Ok((
            [
                (header::CACHE_CONTROL, "no-cache"),
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            ],
            String::from_utf8_lossy(&output).into_owned(),
        )),
        None => Err(error(
            StatusCode::NOT_FOUND,
            format!("task {task_id} not found in session {id}"),
        )),
    }
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Images
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

/// Maximum bytes served from a configured pet sprite sheet. The path is
/// trusted config (never request input), but the cap prevents accidental or
/// maliciously replaced files from becoming unbounded responses.
const PET_SPRITE_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// `GET /api/pet/sprite` — serve only the live config's sprite-sheet path.
/// Missing/unreadable files, disallowed extensions, and oversized files are
/// deliberately indistinguishable as 404; the sprite-only UI remains hidden.
async fn serve_pet_sprite(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    use std::io::Read;

    let path = state
        .factory
        .current_config()
        .and_then(|config| config.pet().and_then(|pet| pet.spritesheet.clone()))
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "pet sprite is not configured"))?;
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| {
            error(
                StatusCode::NOT_FOUND,
                "pet sprite has unsupported extension",
            )
        })?;
    let mime = match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => {
            return Err(error(
                StatusCode::NOT_FOUND,
                "pet sprite has unsupported extension",
            ));
        }
    };
    let file = std::fs::File::open(&path)
        .map_err(|_| error(StatusCode::NOT_FOUND, "pet sprite is unavailable"))?;
    if file
        .metadata()
        .map(|metadata| metadata.len() > PET_SPRITE_MAX_BYTES)
        .unwrap_or(true)
    {
        return Err(error(
            StatusCode::NOT_FOUND,
            "pet sprite exceeds size limit",
        ));
    }
    let mut bytes = Vec::new();
    file.take(PET_SPRITE_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| error(StatusCode::NOT_FOUND, "pet sprite is unreadable"))?;
    if bytes.len() as u64 > PET_SPRITE_MAX_BYTES {
        return Err(error(
            StatusCode::NOT_FOUND,
            "pet sprite exceeds size limit",
        ));
    }
    Ok(([(header::CONTENT_TYPE, mime)], bytes))
}

/// `GET /api/images/{hash}` 的查询参数：可选 `mime`（前端把
/// [`crate::agent::ImagePart`] 的 mime 带回；白名单之外一律回退
/// `application/octet-stream`）。
#[derive(Deserialize)]
pub struct ImageParams {
    pub mime: Option<String>,
}

/// MIME 白名单：与 [`crate::agent::image_mime_from_extension`] 的产出集合
/// 一致（read_image 的 marker 只会带这四种），杜绝把任意字符串写进
/// Content-Type header（header 注入面）。
const IMAGE_MIME_WHITELIST: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// `GET /api/images/{hash}` — 内容寻址图片存取：返回 `read_image` 存入全局
/// image store 的图片字节（`<image store>/<hash>`，见
/// [`crate::agent::image_store_dir`]）。会话数据只存 `hash + mime`（无字节），
/// 前端渲染 `<img>` 时通过本端点取回字节。
///
/// 安全：`hash` 必须是 64 位小写十六进制（SHA-256 hex——文件名即 hash，
/// 内容寻址，无路径拼接；格式校验杜绝路径穿越 / 畸形文件名）；文件不存在
/// 404。认证与其它 `/api/*` 一致（require_auth）；`<img>` 标签无法带
/// Authorization header，前端用 `?token=`（authorized 支持 query token）。
async fn serve_image(
    Path(hash): Path<String>,
    Query(params): Query<ImageParams>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    // [0-9a-f]{64}：仅小写 hex（image_sha256 的产出格式）。大写或含其它
    // 字符一律 400。
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(error(StatusCode::BAD_REQUEST, "invalid image hash"));
    }
    let Some(store) = crate::agent::image_store_dir() else {
        return Err(error(StatusCode::NOT_FOUND, "image store not configured"));
    };
    let Some(bytes) = crate::agent::load_image_bytes(Some(&store), &hash) else {
        return Err(error(
            StatusCode::NOT_FOUND,
            format!("image {hash} not found"),
        ));
    };
    // 内容寻址文件写入时已受 IMAGE_MAX_BYTES 限制；此处防御性兜底。
    if bytes.len() > crate::agent::IMAGE_MAX_BYTES {
        return Err(error(StatusCode::NOT_FOUND, "image exceeds size limit"));
    }
    let mime = params
        .mime
        .filter(|m| IMAGE_MIME_WHITELIST.contains(&m.as_str()))
        .unwrap_or_else(|| "application/octet-stream".to_owned());
    Ok(([(header::CONTENT_TYPE, mime)], bytes))
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// History
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

/// `GET /api/sessions/{id}/history` query parameters, both optional:
/// omit `before_seq` for the head segment, or pass the previous response's
/// `next_before_seq` to page further back. `limit` bounds the page: for the
/// head segment it returns the newest `limit` entries with a cursor at the
/// truncation point (feed it back as `before_seq` to fetch the cut-off
/// older part of the head); for older pages it is passed to the backend
/// for intra-segment paging (~`limit` entries per request, cursor = next
/// older page).
#[derive(Deserialize)]
pub struct HistoryParams {
    pub before_seq: Option<i64>,
    pub limit: Option<usize>,
}

/// One history page: a compaction segment (or an intra-segment slice of
/// it) of [`SessionEntry`] values plus the cursor for the next older page.
/// `next_before_seq` is `Some` when older entries exist (feed it back as
/// `before_seq`), `None` when this was the oldest segment, the oldest page
/// was reached, or the session has no compaction at all.
#[derive(Serialize)]
pub struct HistoryResponse {
    pub entries: Vec<SessionEntry>,
    pub next_before_seq: Option<i64>,
}

/// `GET /api/sessions/{id}/history` — the frontend's initial-render path.
/// Returns the head segment (last compaction + everything after) without
/// `before_seq`, or the entries immediately older than `before_seq` when
/// paging (whole segment with no `limit`, or an intra-segment page of
/// `limit` entries with one). `next_before_seq` is the cursor for the next
/// older page (feed it back to page further back); `null` means the oldest
/// segment/page or no compaction.
///
/// Resolution: the live registry (and live subagent registries) first, then
/// a historical fallback — a registry miss connects a store directly by id
/// so historical sessions and finished subagents (transcripts persisted in
/// the jsonl file / greptime table) are viewable without resuming them
/// first (resuming a finished subagent is meaningless, but its history
/// must stay readable). This mirrors the `btw`/`title`/`pin` "live first,
/// then historical" pattern. 404 is only returned when the transcript
/// truly does not exist: the connected store loads empty (or the id can
/// never exist, e.g. an invalid session name). The SSE events endpoint is
/// intentionally unchanged — streaming needs a live runner, so historical
/// sessions keep 404 there; the frontend only connects SSE after a resume.
async fn session_history(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<HistoryParams>,
) -> Result<Json<HistoryResponse>, (StatusCode, String)> {
    let root = state.factory.root();
    if params
        .limit
        .is_some_and(|limit| limit == 0 || (limit as u64) > i64::MAX as u64)
    {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "limit must be between 1 and i64::MAX",
        ));
    }
    // Live path: both variants carry a session-bound store — the live
    // registry session owns one, and a subagent's `SessionEntry` carries
    // its own (connected at spawn time) — so history reads the same rows
    // the session itself persists to, with no per-request store connect.
    // Historical fallback (registry miss): connect a store by id and read
    // the same rows the live path would; a truly unknown id leaves the
    // store empty → 404, and ids that can never exist (invalid session
    // name) also 404, keeping the previous registry-miss semantics.
    let (store, historical) = resolve_session_store(&state, &id, false).await?;
    let (entries, next_before_seq) = match params.before_seq {
        None => {
            // Head segment, paged: with a `limit` the newest `limit`
            // entries are returned and the cursor is the seq of the
            // oldest entry of that page — the truncation point, fed back
            // as `before_seq` to page into the cut-off part of the head
            // segment (the frontend's 200-entry initial render never
            // loses the gap to the older segments). Without a `limit` the
            // whole head segment is returned and the cursor is the seq of
            // the compaction that opens it (None = the whole session is
            // one head segment).
            let page = store
                .load_head_page(root, &id, params.limit)
                .await
                .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            if historical && page.0.is_empty() {
                return Err(error(
                    StatusCode::NOT_FOUND,
                    format!("session {id} not found"),
                ));
            }
            page
        }
        Some(before_seq) => {
            // Older entries: [prev_comp, before_seq), paged intra-segment
            // by `limit` when present (cursor = oldest seq of the page,
            // crossing into the older segment at a compaction boundary).
            let (entries, cursor) = store
                .load_older(root, &id, before_seq, params.limit)
                .await
                .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            if historical && entries.is_empty() {
                let count = store
                    .count_entries(root, &id)
                    .await
                    .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
                if count == 0 {
                    return Err(error(
                        StatusCode::NOT_FOUND,
                        format!("session {id} not found"),
                    ));
                }
            }
            (entries, cursor)
        }
    };
    Ok(Json(HistoryResponse {
        entries,
        next_before_seq,
    }))
}

/// Resolve a session to a transcript store: the live registry first, then
/// every live session's subagent registry, then a direct store connect for
/// historical sessions (404 when the transcript truly does not exist).
/// Shared by `session_history` and the fork endpoints so all of them accept
/// the same source set — live sessions, subagents, and finished/historical
/// sessions alike.
async fn resolve_session_store(
    state: &AppState,
    id: &str,
    validate_historical: bool,
) -> Result<(SessionStore, bool), (StatusCode, String)> {
    let root = state.factory.root();
    match live(state, id) {
        Ok(SessionRef::Live(session)) => Ok((session.store.clone(), false)),
        Ok(SessionRef::Subagent { entry, .. }) => Ok((entry.store.clone(), false)),
        Err((StatusCode::NOT_FOUND, _)) => {
            if crate::session::validate_session_name(id).is_err() {
                return Err(error(
                    StatusCode::NOT_FOUND,
                    format!("session {id} not found"),
                ));
            }
            let store = SessionStore::connect(state.factory.backend(), root, id)
                .await
                .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            if validate_historical {
                let (entries, _) = store
                    .load_head_page(root, id, Some(1))
                    .await
                    .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
                if entries.is_empty() {
                    return Err(error(
                        StatusCode::NOT_FOUND,
                        format!("session {id} not found"),
                    ));
                }
            }
            Ok((store, true))
        }
        Err(err) => Err(err),
    }
}

/// One forkable history position, as listed by `fork-candidates` and echoed
/// straight back to `POST .../fork`.
#[derive(Serialize)]
struct ForkCandidate {
    /// 1-based index into the full history (`fork_prefix` semantics: the
    /// entry itself is kept, so the fork keeps `entries[0..=at-1]`).
    at: usize,
    /// `load_with_seq` seq — the JSONL line number (0-based) or the real
    /// Greptime seq; passed through untouched as provenance.
    seq: i64,
    /// Display text truncated to ≤ 80 chars (with ellipsis).
    preview: String,
}

/// Display preview for one history entry, truncated to ≤ 80 chars via
/// `agent::preview` (keeps a 2:1 head-to-tail ratio inside the budget).
/// Drives the fork panel's message list.
fn entry_preview(entry: &SessionEntry) -> String {
    const MAX: usize = 80;
    let text = match entry {
        SessionEntry::Message {
            message: Message::User { content, .. },
        } => content.clone(),
        SessionEntry::Message {
            message: Message::Assistant(assistant),
        } => match &assistant.content {
            Some(content) => content.clone(),
            None if !assistant.tool_calls.is_empty() => "（工具调用步骤）".to_owned(),
            None => "（系统条目）".to_owned(),
        },
        SessionEntry::Message {
            message: Message::Tool { name, content, .. },
        } => format!("{name}: {content}"),
        SessionEntry::Compaction { summary, .. } => format!("📦 压缩：{summary}"),
        SessionEntry::GoalUpdated { goal } => match goal {
            Some(goal) => format!("🎯 [{}] {}", goal.status.label(), goal.objective),
            None => "🎯 goal cleared".to_owned(),
        },
        _ => "（系统条目）".to_owned(),
    };
    preview(&text, MAX)
}

/// `GET /api/sessions/{id}/fork-candidates` — every turn boundary in the
/// session's full history, for the web fork panel. Each item carries the
/// 1-based `at` the frontend echoes back to `POST .../fork`, the backend
/// seq, and a truncated preview. Only turn boundaries are listed, so every
/// listed position forks without error. Empty session → 200 `[]`; unknown
/// session → 404 (same resolution and error style as `session_history`).
async fn fork_candidates(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Vec<ForkCandidate>>, (StatusCode, String)> {
    let root = state.factory.root();
    let (store, historical) = resolve_session_store(&state, &id, false).await?;
    let with_seq = store
        .load_with_seq(root, &id)
        .await
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    if historical && with_seq.is_empty() {
        return Err(error(
            StatusCode::NOT_FOUND,
            format!("session {id} not found"),
        ));
    }
    let candidates = with_seq
        .iter()
        .enumerate()
        .filter(|(_, (_, entry))| crate::agent::is_turn_boundary(entry))
        .map(|(index, (seq, entry))| ForkCandidate {
            at: index + 1,
            seq: *seq,
            preview: entry_preview(entry),
        })
        .collect();
    Ok(Json(candidates))
}

#[derive(Deserialize)]
struct ForkBody {
    /// 1-based index into the full history, as listed by `fork-candidates`.
    /// Optional so a missing field is handled as our 400 (axum's Json
    /// extractor would otherwise reject with 422 before the handler runs).
    at: Option<usize>,
}

/// `POST /api/sessions/{id}/fork` — fork the source session's history up
/// to the 1-based turn boundary `at` into a fresh `fork-…` session and
/// register it as live. The new session starts idle (`runner.start(None)`):
/// it sends nothing until the frontend resumes it, mirroring `--fork` in
/// the CLI. The source may be live, a subagent, or historical (same
/// resolution as `fork-candidates` / `session_history`).
/// 201 + `{"id": "fork-…"}`; 400 when `at` is missing or 0; 404 for an
/// unknown source; 409 when `at` is not a forkable turn boundary.
async fn session_fork(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<ForkBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)> {
    let at = match body.at {
        Some(at) if at > 0 => at,
        _ => return Err(error(StatusCode::BAD_REQUEST, "at must be 1-based")),
    };
    // Reserve the final target before any factory await. The reservation
    // covers source resolution, target connect, bootstrap, and publication.
    let target_id = crate::session::new_id_prefixed("fork-");
    let reservation = state.registry.reserve(&target_id).map_err(|_| {
        error(
            StatusCode::CONFLICT,
            format!("session {target_id} was created concurrently"),
        )
    })?;
    // Resolve the source (404 before target build work); the reservation is
    // intentionally held while this awaited lookup completes.
    resolve_session_store(&state, &id, true).await?;
    let built = state
        .factory
        .build_fork(
            &target_id,
            id.clone(),
            Some(at),
            IdlePolicy::WaitForInput,
            // Deliberately NOT the owner-liveness probe of `build_session`:
            // this builds a brand-new `fork-…` session. Unfinished
            // background-task records are scoped to the SOURCE session id,
            // so this fork's own record file/rows are always empty —
            // Consume would take nothing, and injecting a "killed with the
            // process" notice into a fresh fork would be wrong. The source
            // keeps its records untouched either way (fork only reads).
            UnfinishedPolicy::Preserve,
        )
        .await
        .map_err(|e| {
            let message = format!("{e:#}");
            if message.contains("fork point") || message.contains("no completed turn") {
                error(StatusCode::CONFLICT, format!("无法 fork：{message}"))
            } else {
                error(StatusCode::INTERNAL_SERVER_ERROR, message)
            }
        })?;
    let new_id = built.session.clone();
    debug_assert_eq!(new_id, target_id);
    let session = Arc::new(LiveSession {
        handle: built.handle,
        task: built.runner.start(None),
        store: built.store,
        background: built.background,
        sessions: built.sessions,
        model_name: Mutex::new(built.model_name),
        role_name: built.role_name,
        created_at: chrono::Utc::now(),
    });
    reservation.publish(session);
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "id": new_id })),
    ))
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Session summaries (desktop pet)
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

/// How many of the most recent substantive events feed one summary.
const SUMMARY_MAX_EVENTS: usize = 20;

/// Hard cap for one summary model call: summaries are background best-effort
/// work, so a slow/stuck model must never pile up behind a turn.
const SUMMARY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn summary_put(state: &AppState, id: &str, text: String) {
    state.summaries.lock().unwrap().insert(
        id.to_owned(),
        SummaryEntry {
            text,
            at: std::time::SystemTime::now(),
        },
    );
}

fn summary_get(state: &AppState, id: &str) -> Option<SummaryEntry> {
    state.summaries.lock().unwrap().get(id).cloned()
}

/// Count of "substantive" events in the in-memory event log: stable events
/// that persist into history or drive the model (user prompts, assistant
/// text, tool calls/results, errors, background-completion injections).
/// Streaming deltas and transient queue projections are skipped, so a turn
/// that produced only empty chatter never triggers a summary model call.
fn is_substantive(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::UserPrompt(_)
            | AgentEvent::AssistantText(_)
            | AgentEvent::ToolCall { .. }
            | AgentEvent::ToolResult { .. }
            | AgentEvent::Error(_)
            | AgentEvent::BackgroundCompletionNotice { .. }
    )
}

fn substantive_count(events: &[AgentEvent]) -> usize {
    events.iter().filter(|event| is_substantive(event)).count()
}

/// Compress the most recent `max` substantive events into a few short lines
/// for the summarizer model (newest kept, oldest dropped).
fn digest_recent(events: &[AgentEvent], max: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    for event in events.iter().rev() {
        let line = match event {
            AgentEvent::UserPrompt(text) => Some(format!("用户: {}", preview(text, 80))),
            AgentEvent::AssistantText(text) => Some(format!("助手: {}", preview(text, 80))),
            AgentEvent::ToolCall { name, arguments } => {
                Some(format!("调用工具 {name}: {}", preview(arguments, 60)))
            }
            AgentEvent::ToolResult { is_error, content } => {
                let prefix = if *is_error {
                    "工具出错"
                } else {
                    "工具结果"
                };
                Some(format!("{prefix}: {}", preview(content, 60)))
            }
            AgentEvent::Error(text) => Some(format!("错误: {}", preview(text, 80))),
            AgentEvent::BackgroundCompletionNotice { output, .. } => {
                Some(format!("后台任务完成: {}", preview(output, 60)))
            }
            _ => None,
        };
        if let Some(line) = line {
            lines.push(line);
            if lines.len() >= max {
                break;
            }
        }
    }
    lines.reverse();
    lines.join("\n")
}

/// Generate and cache one one-sentence Chinese summary for a live session.
/// Best-effort and fully silent: any failure (empty digest, model error,
/// timeout, empty/absent reply) leaves the previous cache entry untouched
/// and never blocks or fails the session.
async fn generate_summary(state: &AppState, id: &str, digest: &str) {
    if digest.is_empty() {
        return; // 空总结不调模型
    }
    // 用专门的 summarizer 角色（[roles] summarizer，如 deepseek/flash 关思考），
    // 便宜且不占主模型；未配置时回退主模型。
    let mut model = state.factory.summarizer_model();
    let messages = vec![
        Message::System {
            content: "你是 e-agent 桌宠的会话总结器。用一句不超过 30 字的中文，\
概括这个 AI 编程会话最近在做什么。只输出总结本身，不要任何前缀、引号或解释。"
                .into(),
        },
        Message::User {
            content: format!("最近的会话记录：\n{digest}"),
            images: Vec::new(),
        },
    ];
    let Ok(Ok((assistant, usage))) =
        tokio::time::timeout(SUMMARY_TIMEOUT, model.complete(&messages, &[], None)).await
    else {
        return; // 模型失败/超时：静默，不阻塞会话
    };
    // summarizer 调用的 token 用量落盘（kind="summarizer"）：走 workspace
    // 级 meta_store（与 sessions 表同 scope），session 维度用该会话 id；
    // JSONL 后端静默跳过，失败只告警不影响总结。model 名用 summarizer
    // 角色的 profile_key（与 create_meta 存 model 的取法一致）。
    if let Some(usage) = usage {
        let model_name = state.factory.summarizer_model().profile_key();
        if let Err(error) = async {
            let store = meta_store(state).await?;
            store
                .append_usage(
                    state.factory.root(),
                    id,
                    &model_name,
                    "summarizer",
                    None,
                    &usage,
                )
                .await
        }
        .await
        {
            eprintln!("e-agent: cannot record summarizer usage: {error:#}");
        }
    }
    let Some(text) = assistant.content else {
        return;
    };
    let text = text.trim().to_owned();
    if text.is_empty() {
        return;
    }
    summary_put(state, id, text);
}

/// Background per-session turn-end hook: subscribes to the runner's status
/// watch + event broadcast (no runner.rs changes) and summarizes after every
/// Busy→Idle turn whose substantive event count grew. The initial Idle is
/// the baseline, so a fresh session never summarizes; Compacting and
/// Finished are ignored (they are not turns).
///
/// The task deliberately holds only the watch/broadcast *receivers*, not a
/// `SessionHandle`: a handle clone would keep the runner's shared state
/// (event log, command channel) alive after session deletion, so the status
/// sender would never drop and the task would leak. Receivers die with the
/// senders, so the task exits on its own when the session is deleted or the
/// runner exits. On a broadcast lag it resyncs through the registry exactly
/// like `forward_events` does.
fn spawn_summary_listener(state: Arc<AppState>, id: String, session: Arc<LiveSession>) {
    tokio::spawn(async move {
        let (snapshot, mut events, mut status) = session.handle.attach();
        drop(session); // 不持有 handle：见函数注释
        // 自维护计数 + 最近 N 条实质事件窗口（attach 快照 + 增量广播）。
        let mut recent: Vec<AgentEvent> = Vec::new();
        let mut substantive = 0usize;
        for event in snapshot {
            if is_substantive(&event) {
                substantive += 1;
                if recent.len() == SUMMARY_MAX_EVENTS {
                    recent.remove(0);
                }
                recent.push(event);
            }
        }
        let mut baseline = substantive;
        loop {
            tokio::select! {
                changed = status.changed() => {
                    if changed.is_err() {
                        return; // 会话已删除 / 运行器退出：sender drop
                    }
                    let current = status.borrow().clone();
                    match current {
                        SessionStatus::Busy => {
                            // turn 起点基线：只有该 turn 新增的实质事件才触发总结
                            baseline = substantive;
                        }
                        SessionStatus::Idle => {
                            if substantive > baseline {
                                baseline = substantive;
                                generate_summary(
                                    &state,
                                    &id,
                                    &digest_recent(&recent, SUMMARY_MAX_EVENTS),
                                )
                                .await;
                            }
                        }
                        _ => {} // Compacting / Finished：不是 turn，不触发
                    }
                }
                event = events.recv() => match event {
                    Ok(event) => {
                        if is_substantive(&event) {
                            substantive += 1;
                            if recent.len() == SUMMARY_MAX_EVENTS {
                                recent.remove(0);
                            }
                            recent.push(event);
                        }
                    }
                    Err(RecvError::Lagged(_)) => {
                        // 广播落后：经 registry 重新 attach 恢复（同 forward_events）。
                        let Some(session) = state.registry.get(&id) else { return };
                        let (snapshot, new_events, new_status) = session.handle.attach();
                        substantive = substantive_count(&snapshot);
                        events = new_events;
                        status = new_status;
                        if matches!(*status.borrow(), SessionStatus::Busy) {
                            baseline = substantive;
                        } else if matches!(*status.borrow(), SessionStatus::Idle)
                            && substantive > baseline
                        {
                            // 落后期间错过了一次完整 turn：补上这次总结。
                            baseline = substantive;
                            generate_summary(
                                &state,
                                &id,
                                &digest_recent(&snapshot, SUMMARY_MAX_EVENTS),
                            )
                            .await;
                        }
                    }
                    Err(RecvError::Closed) => return,
                },
            }
        }
    });
}

/// `GET /api/sessions/{id}/summary` — the desktop pet's click-time read of
/// the per-turn summary cache. `{"summary": "...", "at": "<rfc3339>"}`;
/// 404 when the session is unknown or no summary has been generated yet
/// (the pet then falls back to its catchphrases).
async fn session_summary(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let session = live(&state, &id)
        .map_err(|(status, message)| (status, Json(serde_json::json!({ "error": message }))))?;
    let Some(entry) = summary_get(&state, &id) else {
        // 冷缓存（server 重启后 / 会话尚无完整 turn）：按需触发一次后台
        // 生成，并返回 generating 标记——桌宠据此提示"稍后再点"，而不是
        // 静默降级成随机台词。防抖：同一会话同时只允许一个生成在跑。
        let (snapshot, _, _) = session.handle().attach();
        let digest = digest_recent(&snapshot, SUMMARY_MAX_EVENTS);
        if digest.is_empty() {
            // 会话还没有任何实质活动：无可总结，提示而不是生成。
            return Err((
                StatusCode::NOT_FOUND,
                Json(
                    serde_json::json!({ "error": format!("session {id} has no activity yet"), "no_activity": true }),
                ),
            ));
        }
        let mut pending = state.summary_pending.0.lock().unwrap();
        let generating = if !pending.contains(&id) {
            pending.insert(id.clone());
            drop(pending);
            let state = state.clone();
            let id2 = id.clone();
            tokio::spawn(async move {
                generate_summary(&state, &id2, &digest).await;
                state.summary_pending.0.lock().unwrap().remove(&id2);
            });
            true
        } else {
            drop(pending);
            true // 已有生成在跑：同样按 generating 提示
        };
        return Err((
            StatusCode::NOT_FOUND,
            Json(
                serde_json::json!({ "error": format!("session {id} has no summary yet"), "generating": generating }),
            ),
        ));
    };
    let at = chrono::DateTime::<chrono::Utc>::from(entry.at).to_rfc3339();
    Ok(Json(serde_json::json!({ "summary": entry.text, "at": at })))
}

/// One aggregate usage row in the `GET /api/sessions/{id}/usage` response:
/// totals per (session_id, model, kind) from the persisted `usage_entries`
/// table (Greptime/SQLite).
#[derive(Serialize)]
struct UsageRowJson {
    session_id: String,
    model: String,
    kind: String,
    input_tokens: u64,
    output_tokens: u64,
}

/// `GET /api/sessions/{id}/usage` — the web UI usage line's persisted
/// half: token totals for the session PLUS its subagent children (sessions
/// whose `parent_session_id` = `{id}`, found via `list_meta`), aggregated
/// from the `usage_entries` table so the numbers survive a server restart
/// (the live SSE `Usage` event only carries in-process counters).
///
/// Response: `{"input_tokens": N, "output_tokens": M, "rows": [...]}` with
/// the totals summed over every returned row. The session need not be live
/// or even known: unknown ids return `200` with zero totals (the frontend
/// then falls back to live-only counters) — the route answers a storage
/// question, so an unknown id is an empty answer, not an error. JSONL
/// backend: no usage table → always zero totals.
async fn session_usage(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let root = state.factory.root();
    // 子会话集合：元数据里 parent_session_id == id 的会话（含已结束的）。
    // list_meta 失败降级为只查本会话——用量行不应因元数据问题整体消失。
    let mut ids = vec![id.clone()];
    match async {
        let store = meta_store(&state).await?;
        store.list_meta(root).await
    }
    .await
    {
        Ok(metas) => {
            for meta in metas {
                if meta.parent_session_id.as_deref() == Some(id.as_str()) {
                    ids.push(meta.session_id);
                }
            }
        }
        Err(error) => {
            eprintln!("e-agent: cannot list session metadata for usage: {error:#}");
        }
    }
    let rows = match async {
        let store = meta_store(&state).await?;
        store.usage_for_sessions(root, &ids).await
    }
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            eprintln!("e-agent: cannot query session usage: {error:#}");
            Vec::new()
        }
    };
    let input: u64 = rows.iter().map(|r| r.input_tokens).sum();
    let output: u64 = rows.iter().map(|r| r.output_tokens).sum();
    let rows_json: Vec<UsageRowJson> = rows
        .into_iter()
        .map(|r| UsageRowJson {
            session_id: r.session_id,
            model: r.model,
            kind: r.kind,
            input_tokens: r.input_tokens,
            output_tokens: r.output_tokens,
        })
        .collect();
    Json(serde_json::json!({
        "input_tokens": input,
        "output_tokens": output,
        "rows": rows_json,
    }))
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// SSE
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

/// `GET /api/sessions/{id}/events` — one `event: snapshot` carrying the
/// recent event array (newest `SNAPSHOT_MAX`, a fallback when the history
/// route fails), one initial `event: status`, then live frames named after
/// the [`AgentEvent`] variant (CamelCase: `UserPrompt`, `AssistantDelta`,
/// `ToolCall`, …) plus `event: status` on state changes and a `: ping`
/// comment every 15s. Events are forwarded over a bounded queue
/// (`SSE_CHANNEL_CAPACITY`): a client that stalls long enough to fill it is
/// disconnected (the frontend reconnects after 3s). If a client ever falls
/// behind the broadcast buffer, a fresh `event: resync` with the recent
/// event array is re-sent (the frontend force-replaces its transcript) and
/// the stream continues.
async fn session_events(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Response, (StatusCode, String)> {
    let session = live(&state, &id)?;
    let (snapshot, live, status) = session.handle().attach();
    let (tx, rx) = mpsc::channel::<Result<Event, Error>>(SSE_CHANNEL_CAPACITY);
    let shutdown = state.shutdown.subscribe();
    tokio::spawn(forward_events(
        state,
        id,
        tail_snapshot(snapshot),
        live,
        status,
        shutdown,
        tx,
    ));
    let mut response = Sse::new(SseReceiver(rx)).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-cache"),
    );
    Ok(response)
}

/// tokio's `Receiver` does not implement `Stream` on its own; this adapter
/// (a few lines, no extra dependency) exposes `poll_recv`.
struct SseReceiver(mpsc::Receiver<Result<Event, Error>>);

impl Stream for SseReceiver {
    type Item = Result<Event, Error>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        self.0.poll_recv(cx)
    }
}

async fn forward_events(
    state: Arc<AppState>,
    id: String,
    snapshot: Vec<AgentEvent>,
    mut events: broadcast::Receiver<AgentEvent>,
    mut status: watch::Receiver<SessionStatus>,
    mut shutdown: watch::Receiver<()>,
    tx: mpsc::Sender<Result<Event, Error>>,
) {
    // Bounded queue: a full queue means the client is too slow to keep up —
    // drop the connection instead of buffering without bound (the frontend
    // reconnects after 3s).
    let send = |event: Result<Event, Error>| tx.try_send(event).is_ok();
    if !send(snapshot_event(&snapshot)) {
        return;
    }
    if !send(status_event(&status.borrow().clone())) {
        return;
    }
    let mut tick = tokio::time::interval(HEARTBEAT);
    tick.tick().await; // interval's first tick fires immediately; skip it
    loop {
        tokio::select! {
            // 服务器 shutdown（Ctrl-C）：流立即自关闭，SSE 连接随之断开，
            // graceful drain 毫秒级完成（SHUTDOWN_DRAIN_TIMEOUT 只是兜底）。
            // sender 持有在 AppState 里，正常运行时不会 Err；真掉了也当
            // shutdown 处理，直接结束。
            _ = shutdown.changed() => return,
            changed = status.changed() => match changed {
                Ok(_) => {
                    if !send(status_event(&status.borrow().clone())) {
                        return;
                    }
                }
                // Status sender dropped: the session was deleted. End the stream.
                Err(_) => return,
            },
            event = events.recv() => match event {
                Ok(event) => {
                    if !send(live_event(&event)) {
                        return;
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    // Client fell behind the broadcast buffer; resync with a
                    // fresh `event: resync` (unlike `snapshot`, the frontend
                    // never skips it — it force-replaces the transcript). The
                    // resync carries the recent log tail (capped at
                    // SNAPSHOT_MAX; it only needs to cover the broadcast gap
                    // with margin). The session is re-resolved through the
                    // unified lookup so a subagent stream resyncs exactly
                    // like a registry stream; a deleted session (registry
                    // entry or subagent `Sessions` entry gone) ends the
                    // stream.
                    let Ok(session) = live(&state, &id) else { return };
                    let (snapshot, new_events, new_status) = session.handle().attach();
                    if !send(resync_event(&tail_snapshot(snapshot))) {
                        return;
                    }
                    if !send(status_event(&new_status.borrow().clone())) {
                        return;
                    }
                    events = new_events;
                    status = new_status;
                }
                // Broadcast sender dropped: the session was deleted.
                Err(RecvError::Closed) => return,
            },
            _ = tick.tick() => {
                if !send(Ok(Event::default().comment("ping"))) {
                    return;
                }
            }
        }
    }
}

fn snapshot_event(events: &[AgentEvent]) -> Result<Event, Error> {
    Event::default().event("snapshot").json_data(events)
}

/// Lag resync: the recent event log tail re-sent when a client falls behind
/// the broadcast buffer (capped at `SNAPSHOT_MAX`). The frontend's `resync`
/// branch force-replaces the transcript (unlike `snapshot`, which it skips
/// once history rendered).
fn resync_event(events: &[AgentEvent]) -> Result<Event, Error> {
    Event::default().event("resync").json_data(events)
}

fn live_event(event: &AgentEvent) -> Result<Event, Error> {
    Event::default()
        .event(event_name(event))
        .json_data(event_payload(event))
}

fn status_event(status: &SessionStatus) -> Result<Event, Error> {
    Event::default()
        .event("status")
        .json_data(status_json(status))
}

/// SSE event name for a live [`AgentEvent`]: the Rust variant name in
/// CamelCase, matching the frontend's `applyLiveEvent` switch
/// (`UserPrompt`, `AssistantDelta`, `ToolCall`, …). This deliberately
/// differs from the serde `type` tag (`user_prompt` vs `UserPrompt`).
fn event_name(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::PromptQueued(_) => "PromptQueued",
        AgentEvent::PromptConsumed => "PromptConsumed",
        AgentEvent::UserPrompt(_) => "UserPrompt",
        AgentEvent::AssistantText(_) => "AssistantText",
        AgentEvent::AssistantDelta(_) => "AssistantDelta",
        AgentEvent::ReasoningDelta(_) => "ReasoningDelta",
        AgentEvent::ToolCall { .. } => "ToolCall",
        AgentEvent::ToolResult { .. } => "ToolResult",
        AgentEvent::Notice(_) => "Notice",
        AgentEvent::Error(_) => "Error",
        AgentEvent::BackgroundCompleted { .. } => "BackgroundCompleted",
        AgentEvent::BackgroundCompletionNotice { .. } => "BackgroundCompletionNotice",
        AgentEvent::GoalUpdated { .. } => "GoalUpdated",
        AgentEvent::Usage { .. } => "Usage",
    }
}

/// Live-event payload for the web frontend: a flat object (or bare string)
/// whose fields match the keys `pickText` / the per-event handlers read.
/// The `{type,data}` serde shape would leak the snake_case `type` tag into
/// `pickText`'s first-string fallback (Object.values would return
/// `"user_prompt"` before the real text), so the wire payload is derived
/// per-variant instead.
fn event_payload(event: &AgentEvent) -> serde_json::Value {
    use serde_json::json;
    match event {
        AgentEvent::PromptQueued(text)
        | AgentEvent::UserPrompt(text)
        | AgentEvent::AssistantText(text)
        | AgentEvent::Notice(text) => json!({ "text": text }),
        AgentEvent::AssistantDelta(text) | AgentEvent::ReasoningDelta(text) => {
            json!({ "delta": text })
        }
        AgentEvent::PromptConsumed => json!({}),
        AgentEvent::Error(text) => json!({ "error": text }),
        AgentEvent::ToolCall { name, arguments } => {
            json!({ "name": name, "arguments": arguments })
        }
        AgentEvent::ToolResult { is_error, content } => {
            json!({ "is_error": is_error, "content": content })
        }
        AgentEvent::BackgroundCompleted {
            id,
            output,
            label,
            started_at_ms,
            duration_ms,
            exit_code,
            signal,
            status,
            kind,
        }
        | AgentEvent::BackgroundCompletionNotice {
            id,
            output,
            label,
            started_at_ms,
            duration_ms,
            exit_code,
            signal,
            status,
            kind,
        } => json!({
            "id": id,
            "output": output,
            "label": label,
            "started_at_ms": started_at_ms,
            "duration_ms": duration_ms,
            "exit_code": exit_code,
            "signal": signal,
            "status": status,
            "kind": kind,
        }),
        AgentEvent::GoalUpdated { goal } => json!({ "goal": goal }),
        AgentEvent::Usage {
            context_input,
            context_window,
            session,
        } => {
            json!({
                "context_input": context_input,
                "context_window": context_window,
                "session": session
            })
        }
    }
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Static UI
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

/// Minimal placeholder until the frontend task lands `src/ui/index.html`
/// (which the build script then compiles in as `web_ui`).
#[cfg(not(web_ui))]
const PLACEHOLDER_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>e-agent server</title>
<style>body{font-family:system-ui,sans-serif;max-width:40rem;margin:3rem auto;padding:0 1rem;line-height:1.6}code{background:#f0f0f0;padding:.1em .3em;border-radius:4px}</style>
</head>
<body>
<h1>e-agent server running</h1>
<p>The web UI has not been compiled into this binary yet (src/ui/index.html is missing).</p>
<p>Server token: <code>__TOKEN__</code></p>
<p>Authenticate API requests with <code>Authorization: Bearer &lt;token&gt;</code> or <code>?token=&lt;token&gt;</code>.</p>
</body>
</html>
"#;

#[cfg(not(web_ui))]
fn html_headers() -> [(header::HeaderName, header::HeaderValue); 2] {
    [
        (
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/html; charset=utf-8"),
        ),
        // Stable URL, no content hash: never let a browser cache the
        // placeholder either.
        (
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-store"),
        ),
    ]
}

#[cfg(not(web_ui))]
async fn index(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    (
        html_headers(),
        PLACEHOLDER_HTML.replace("__TOKEN__", &state.token),
    )
}

#[cfg(web_ui)]
fn assemble(read: impl Fn(&str) -> Result<String, String>) -> Result<String, String> {
    read("index.html").and_then(|skeleton| {
        let katex_css = read("vendor/katex.min.css")?;
        let css = read("style.css")?;
        let vendor_js = read("vendor/marked.min.js")?;
        let pet_html = read("pet.html")?;
        // app.js 已按功能域拆为多文件：同一 <script> 内按序拼接（顶层声明跨
        // 文件全局可见，函数互调无需 export/import；事件绑定与 init 在最后一个
        // 文件 sse.js 尾部执行）。顺序即依赖：核心 → 渲染 → 会话 → 任务 → SSE；
        // 任一文件读失败都会走 `?` 冒泡 → 500（与原来单文件读取一致）。
        let mut app_js = String::new();
        for name in ["app.js", "render.js", "sessions.js", "tasks.js", "sse.js"] {
            app_js.push_str(&read(name)?);
            app_js.push('\n');
        }
        Ok(skeleton
            .replace("/*__KATEX_CSS__*/", &katex_css)
            .replace("/*__CSS__*/", &css)
            .replace("/*__JS_VENDOR__*/", &vendor_js)
            .replace("/*__JS_APP__*/", &app_js)
            .replace("<!--__PET__-->", &pet_html))
    })
}

/// Release compile-time asset reader: the ten UI files are baked into the
/// binary with `include_str!`, so the server keeps serving the UI after the
/// source tree (and its `src/ui` directory) is deleted. gated to release +
/// tests only — a plain debug build must keep the on-disk read (missing
/// file → 500) semantics, not silently fall back to an embedded copy.
#[cfg(all(web_ui, any(not(debug_assertions), test)))]
fn read_embedded_ui(name: &str) -> Result<String, String> {
    let source = match name {
        "index.html" => include_str!("ui/index.html"),
        "vendor/katex.min.css" => include_str!("ui/vendor/katex.min.css"),
        "style.css" => include_str!("ui/style.css"),
        "vendor/marked.min.js" => include_str!("ui/vendor/marked.min.js"),
        "pet.html" => include_str!("ui/pet.html"),
        "app.js" => include_str!("ui/app.js"),
        "render.js" => include_str!("ui/render.js"),
        "sessions.js" => include_str!("ui/sessions.js"),
        "tasks.js" => include_str!("ui/tasks.js"),
        "sse.js" => include_str!("ui/sse.js"),
        _ => return Err(format!("unknown embedded UI asset: {name}")),
    };
    Ok(source.to_owned())
}

/// Release-only process-lifetime cache of the assembled UI. The response
/// body is served as a `&'static str` (via `Body`'s `From<&'static str>`),
/// so a request never clones the cached HTML.
#[cfg(all(web_ui, not(debug_assertions)))]
static EMBEDDED_UI_HTML: std::sync::OnceLock<String> = std::sync::OnceLock::new();

#[cfg(web_ui)]
async fn index() -> impl IntoResponse {
    use axum::http::Response as HttpResponse;

    #[cfg(debug_assertions)]
    {
        // Dev-friendly: read the UI skeleton from disk and inline the
        // CSS/JS pieces on every request, so frontend edits (style.css,
        // app.js, vendor libs) show up on refresh without recompiling. The
        // response stays a single self-contained HTML file. Located via
        // CARGO_MANIFEST_DIR (stable regardless of the process cwd).
        let ui = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui");
        let read = |name: &str| -> Result<String, String> {
            std::fs::read_to_string(ui.join(name))
                .map_err(|e| format!("cannot read {}: {e}", ui.join(name).display()))
        };
        match assemble(read) {
            Ok(html) => HttpResponse::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                // Never cache the dev UI: browsers would serve a stale HTML
                // after a reload.
                .header(header::CACHE_CONTROL, "no-store")
                .body(html)
                .unwrap(),
            Err(error) => HttpResponse::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .body(format!(
                    "<!doctype html><meta charset=\"utf-8\"><title>e-agent UI error</title>\
                     <body style=\"font-family:system-ui,sans-serif;padding:2rem\">\
                     <h1>Cannot assemble UI</h1><pre>{error}</pre></body>"
                ))
                .unwrap(),
        }
    }

    #[cfg(not(debug_assertions))]
    {
        // Release: the UI is compiled into the binary (read_embedded_ui),
        // so it survives a deleted source tree. Assemble once into
        // EMBEDDED_UI_HTML and serve the cached string; the `.expect`
        // mirrors the dev 500 — the embedded asset table must stay in sync
        // with `assemble`'s read order.
        let html = EMBEDDED_UI_HTML.get_or_init(|| {
            assemble(read_embedded_ui).expect("embedded UI asset table must match assemble()")
        });
        HttpResponse::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            // Stable URL, no content hash: never cache this either.
            .header(header::CACHE_CONTROL, "no-store")
            .body(Body::from(html.as_str()))
            .unwrap()
    }
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Tests
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::TaskDisplayMeta;

    fn test_state_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("e-agent-server-tests-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn list_sessions_slow_log_is_silent_below_threshold_and_clean() {
        let timing = ListSessionsTiming {
            total_ms: LIST_SESSIONS_SLOW_THRESHOLD.as_millis() - 1,
            ..ListSessionsTiming::default()
        };
        assert!(list_sessions_slow_log(timing, false, 1, 2, 3, 0).is_none());
    }

    #[test]
    fn list_sessions_slow_log_fires_at_threshold() {
        let timing = ListSessionsTiming {
            total_ms: LIST_SESSIONS_SLOW_THRESHOLD.as_millis(),
            ..ListSessionsTiming::default()
        };
        let message = list_sessions_slow_log(timing, false, 1, 2, 3, 0).unwrap();
        assert!(message.starts_with("e-agent: GET /api/sessions slow:"));
        assert!(message.contains("total=1000ms"));
        assert!(message.contains("connect=0ms"));
        assert!(message.contains("counts(live=1 historical=2 merged=3 subagent_labels=0)"));
    }

    #[test]
    fn list_sessions_slow_log_fires_when_degraded_even_if_fast() {
        let message =
            list_sessions_slow_log(ListSessionsTiming::default(), true, 0, 0, 0, 0).unwrap();
        assert!(message.starts_with("e-agent: GET /api/sessions degraded:"));
    }

    #[test]
    fn list_meta_diagnostics_format_contains_only_safe_backend_fields() {
        let greptime = ListMetaDiagnostics {
            backend: "greptime",
            facade_lock_wait_ms: 11,
            backend_operation_ms: 22,
            query_ms: 33,
            row_decode_ms: 44,
            logical_rows: 55,
            ..Default::default()
        };
        assert_eq!(
            greptime.format_for_log(),
            "backend=greptime facade_lock_wait=11ms backend_op=22ms query=33ms decode=44ms rows=55"
        );

        let sqlite = ListMetaDiagnostics {
            backend: "sqlite",
            facade_lock_wait_ms: 1,
            connection_lock_wait_ms: 2,
            backend_operation_ms: 3,
            query_iteration_ms: 4,
            row_decode_ms: 5,
            logical_rows: 6,
            ..Default::default()
        };
        assert_eq!(
            sqlite.format_for_log(),
            "backend=sqlite facade_lock_wait=1ms conn_lock_wait=2ms backend_op=3ms query_iter=4ms decode=5ms rows=6"
        );

        let jsonl = ListMetaDiagnostics {
            backend: "jsonl",
            backend_operation_ms: 7,
            filesystem_parse_ms: 8,
            sidecars_seen: 9,
            sidecars_opened: 10,
            logical_rows: 11,
            ..Default::default()
        };
        assert_eq!(
            jsonl.format_for_log(),
            "backend=jsonl facade_lock_wait=0ms backend_op=7ms fs_parse=8ms sidecars(seen=9 opened=10) rows=11"
        );
    }

    #[test]
    fn list_sessions_slow_log_includes_backend_diagnostics() {
        let timing = ListSessionsTiming {
            total_ms: LIST_SESSIONS_SLOW_THRESHOLD.as_millis(),
            connect_ms: 7,
            list_meta_diagnostics: Some(ListMetaDiagnostics {
                backend: "sqlite",
                connection_lock_wait_ms: 12,
                logical_rows: 34,
                ..Default::default()
            }),
            ..Default::default()
        };
        let message = list_sessions_slow_log(timing, false, 0, 34, 34, 0).unwrap();
        assert!(message.contains("backend=sqlite"));
        assert!(message.contains("conn_lock_wait=12ms"));
        assert!(message.contains("rows=34"));
        assert!(!message.contains("session_id"));
        assert!(!message.contains("SELECT"));
    }

    #[test]
    fn state_dir_resolves_xdg_then_home_fallback() {
        let xdg = std::ffi::OsStr::new("/tmp/xdg");
        let home = std::ffi::OsStr::new("/tmp/home");
        assert_eq!(
            state_dir_inner(Some(xdg), Some(home)),
            Some(PathBuf::from("/tmp/xdg/e-agent"))
        );
        assert_eq!(
            state_dir_inner(None, Some(home)),
            Some(PathBuf::from("/tmp/home/.local/state/e-agent"))
        );
        assert_eq!(state_dir_inner(None, None), None);
    }

    #[test]
    fn token_is_written_private_and_readable() {
        let dir = test_state_dir();
        let path = dir.join("server.token");
        let token = load_or_create_token_at(path.clone()).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(token, contents);
        assert!(!token.is_empty());
        assert!(!token.contains('/') && !token.contains('+'));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_is_reused_across_calls() {
        // Own unique dir: test_state_dir() is shared per-process and other
        // tests remove it at the end, which would race this one in parallel.
        let dir =
            std::env::temp_dir().join(format!("e-agent-server-test-reuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server.token");
        let first = load_or_create_token_at(path.clone()).unwrap();
        let second = load_or_create_token_at(path.clone()).unwrap();
        assert_eq!(
            first, second,
            "existing token must be reused, not regenerated"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn killed_notice_text_skips_when_nothing_died() {
        // 自己的死行与子会话汇总都为空 → None：扫描跳过，不注入空 Notice。
        assert_eq!(killed_notice_text(&[], &[]), None);
        assert_eq!(killed_notice_text(&[], &[]), None);
    }

    #[test]
    fn killed_notice_text_counts_own_and_subagent_tasks() {
        // 父会话自己的 2 个死 bash 行 + 2 个子会话（分别 2 / 1 条死
        // delegate 行）→ 一条汇总：N = 2 + 2 + 1 = 5，自己的任务逐行列出，
        // 子会话 label 以「、」连接成一行。
        let own = vec![
            "task 1: npm run dev".to_owned(),
            "task 2: sleep 100".to_owned(),
        ];
        let killed = vec![
            ("btw: 分析日志".to_owned(), 2usize),
            ("task: 修 bug".to_owned(), 1usize),
        ];
        let text = killed_notice_text(&own, &killed).expect("non-empty notice");
        assert!(
            text.starts_with(
                "[e-agent exited with 5 background task(s) still running; \
                 they were killed with the process. Re-run them if still needed:]"
            ),
            "got: {text}"
        );
        assert!(text.contains("task 1: npm run dev"));
        assert!(text.contains("task 2: sleep 100"));
        assert!(text.contains("被杀子会话: btw: 分析日志、task: 修 bug"));
    }

    #[test]
    fn killed_notice_text_handles_only_subagents_or_only_own() {
        // 只有子会话被杀：N 只计子会话的 delegate 行数。
        let only_subagents =
            killed_notice_text(&[], &[("sub-label".to_owned(), 3usize)]).expect("non-empty notice");
        assert!(
            only_subagents.starts_with("[e-agent exited with 3 background task(s) still running")
        );
        assert!(only_subagents.contains("被杀子会话: sub-label"));
        // 只有自己的死行：与旧格式完全一致（inject_killed_notice 的行为）。
        let only_own =
            killed_notice_text(&["task 7: cargo test".to_owned()], &[]).expect("non-empty notice");
        assert!(only_own.starts_with("[e-agent exited with 1 background task(s) still running"));
        assert!(only_own.contains("task 7: cargo test"));
        assert!(!only_own.contains("被杀子会话"));
    }

    #[test]
    fn authorized_matches_bearer_and_query_token() {
        use axum::http::Request;
        let token = "sekrit";
        let with_bearer = Request::builder()
            .uri("http://x/api/sessions")
            .header(header::AUTHORIZATION, "Bearer sekrit")
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(authorized(token, &with_bearer));
        let wrong_bearer = Request::builder()
            .uri("http://x/api/sessions")
            .header(header::AUTHORIZATION, "Bearer nope")
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(!authorized(token, &wrong_bearer));
        let with_query = Request::builder()
            .uri("http://x/api/sessions?token=sekrit")
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(authorized(token, &with_query));
        let wrong_query = Request::builder()
            .uri("http://x/api/sessions?token=nope")
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(!authorized(token, &wrong_query));
        let none = Request::builder()
            .uri("http://x/api/sessions")
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(!authorized(token, &none));
    }

    #[tokio::test]
    async fn cors_preflight_and_response_headers() {
        use tower::util::ServiceExt;
        let app = router(Arc::new(AppState {
            factory: crate::session_factory::SessionFactory::test_factory(std::env::temp_dir()),
            registry: Arc::new(SessionRegistry::default()),
            token: "sekrit".to_owned(),
            meta_store: SessionStore::Jsonl,
            summaries: Arc::new(Mutex::new(HashMap::new())),
            summary_pending: Arc::new(SummaryPending(Mutex::new(HashSet::new()))),
            shutdown: watch::channel(()).0,
        }));
        // Preflight: answered without auth, 204 + CORS headers.
        let preflight = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/api/sessions")
                    .header(header::ORIGIN, "http://127.0.0.1:18766")
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                    .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "authorization")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            preflight
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "*"
        );
        // Authed GET: normal response + CORS header so the browser surfaces
        // real status codes instead of a generic CORS failure.
        let get = app
            .oneshot(
                Request::builder()
                    .uri("/api/sessions")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(get.status(), StatusCode::OK);
        assert_eq!(
            get.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "*"
        );
        // 动态 API 响应必须带 Cache-Control: no-store（防浏览器缓存旧实例
        // 响应 → stale 列表误标；见 cache_control_middleware）。
        assert_eq!(
            get.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store",
            "authed GET /api/sessions must be no-store"
        );
    }

    #[tokio::test]
    async fn api_responses_are_never_cached() {
        use tower::util::ServiceExt;
        let app = router(Arc::new(AppState {
            factory: crate::session_factory::SessionFactory::test_factory(std::env::temp_dir()),
            registry: Arc::new(SessionRegistry::default()),
            token: "sekrit".to_owned(),
            meta_store: SessionStore::Jsonl,
            summaries: Arc::new(Mutex::new(HashMap::new())),
            summary_pending: Arc::new(SummaryPending(Mutex::new(HashSet::new()))),
            shutdown: watch::channel(()).0,
        }));
        // 未认证也带 no-store：401 同样不该被浏览器缓存复用。
        let unauth = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/sessions")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            unauth.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        // 其它动态端点（history）同规约。
        let history = app
            .oneshot(
                Request::builder()
                    .uri("/api/sessions/whatever/history")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            history.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store",
            "GET /api/sessions/{{id}}/history must be no-store"
        );
    }

    #[test]
    fn status_string_is_camel_case_frontend_values() {
        use crate::runner::SessionResult;
        assert_eq!(status_string(&SessionStatus::Idle), "Idle");
        assert_eq!(status_string(&SessionStatus::Busy), "Busy");
        assert_eq!(status_string(&SessionStatus::Compacting), "Compacting");
        // Finished detail is intentionally dropped: the frontend renders the
        // chip from the bare string and never reads the result payload.
        assert_eq!(
            status_string(&SessionStatus::Finished(SessionResult::Completed(Some(
                "hi".into()
            )))),
            "Finished"
        );
        assert_eq!(
            status_string(&SessionStatus::Finished(SessionResult::Failed(
                "boom".into()
            ))),
            "Finished"
        );
    }

    #[test]
    fn status_json_is_object_with_camel_case_status() {
        use serde_json::json;
        // The frontend does `applyStatus(JSON.parse(data).status)`: the frame
        // must be an object carrying the CamelCase string.
        assert_eq!(status_json(&SessionStatus::Idle), json!({"status": "Idle"}));
        assert_eq!(status_json(&SessionStatus::Busy), json!({"status": "Busy"}));
        assert_eq!(
            status_json(&SessionStatus::Compacting),
            json!({"status": "Compacting"})
        );
        assert_eq!(
            status_json(&SessionStatus::Finished(
                crate::runner::SessionResult::Cancelled
            )),
            json!({"status": "Finished"})
        );
    }

    #[test]
    fn agent_events_serialize_tagged() {
        let value = serde_json::to_value(AgentEvent::ToolCall {
            name: "bash".into(),
            arguments: "ls".into(),
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({"type": "tool_call", "data": {"name": "bash", "arguments": "ls"}})
        );
        assert_eq!(
            serde_json::to_value(AgentEvent::Notice("hi".into())).unwrap(),
            serde_json::json!({"type": "notice", "data": "hi"})
        );
    }

    /// Frontend contract: live SSE frames are named after the Rust variant
    /// (CamelCase) — `applyLiveEvent` switches on these exact strings.
    #[test]
    fn event_name_is_camel_case_variant() {
        let name = |event: &AgentEvent| event_name(event);
        assert_eq!(name(&AgentEvent::PromptQueued("x".into())), "PromptQueued");
        assert_eq!(name(&AgentEvent::PromptConsumed), "PromptConsumed");
        assert_eq!(name(&AgentEvent::UserPrompt("x".into())), "UserPrompt");
        assert_eq!(
            name(&AgentEvent::AssistantText("x".into())),
            "AssistantText"
        );
        assert_eq!(
            name(&AgentEvent::AssistantDelta("x".into())),
            "AssistantDelta"
        );
        assert_eq!(
            name(&AgentEvent::ReasoningDelta("x".into())),
            "ReasoningDelta"
        );
        assert_eq!(
            name(&AgentEvent::ToolCall {
                name: "bash".into(),
                arguments: "ls".into()
            }),
            "ToolCall"
        );
        assert_eq!(
            name(&AgentEvent::ToolResult {
                is_error: false,
                content: "o".into()
            }),
            "ToolResult"
        );
        assert_eq!(name(&AgentEvent::Notice("x".into())), "Notice");
        assert_eq!(name(&AgentEvent::Error("x".into())), "Error");
        assert_eq!(
            name(&AgentEvent::BackgroundCompleted {
                id: 1,
                output: "o".into(),
                label: None,
                started_at_ms: None,
                duration_ms: None,
                exit_code: None,
                signal: None,
                status: None,
                kind: None,
            }),
            "BackgroundCompleted"
        );
        assert_eq!(
            name(&AgentEvent::BackgroundCompletionNotice {
                id: 1,
                output: "o".into(),
                label: None,
                started_at_ms: None,
                duration_ms: None,
                exit_code: None,
                signal: None,
                status: None,
                kind: None,
            }),
            "BackgroundCompletionNotice"
        );
        assert_eq!(
            name(&AgentEvent::Usage {
                context_input: 1,
                context_window: None,
                session: crate::agent::Usage {
                    input_tokens: 1,
                    output_tokens: 2,
                    ..Default::default()
                },
            }),
            "Usage"
        );
    }

    /// Frontend contract: live payloads are flat — fields at the top level,
    /// matching the keys `pickText` and the per-event handlers read. The
    /// `{type,data}` serde shape would make `pickText`'s Object.values
    /// fallback return the snake_case `type` tag instead of the real text.
    #[test]
    fn event_payload_is_flat_for_frontend() {
        use serde_json::json;
        assert_eq!(
            event_payload(&AgentEvent::UserPrompt("hello".into())),
            json!({"text": "hello"})
        );
        assert_eq!(
            event_payload(&AgentEvent::AssistantText("done".into())),
            json!({"text": "done"})
        );
        assert_eq!(
            event_payload(&AgentEvent::AssistantDelta("正在".into())),
            json!({"delta": "正在"})
        );
        assert_eq!(
            event_payload(&AgentEvent::ReasoningDelta("推理".into())),
            json!({"delta": "推理"})
        );
        assert_eq!(
            event_payload(&AgentEvent::ToolCall {
                name: "bash".into(),
                arguments: "ls".into()
            }),
            json!({"name": "bash", "arguments": "ls"})
        );
        assert_eq!(
            event_payload(&AgentEvent::ToolResult {
                is_error: true,
                content: "boom".into()
            }),
            json!({"is_error": true, "content": "boom"})
        );
        assert_eq!(
            event_payload(&AgentEvent::Notice("hi".into())),
            json!({"text": "hi"})
        );
        assert_eq!(
            event_payload(&AgentEvent::Error("bad".into())),
            json!({"error": "bad"})
        );
        assert_eq!(
            event_payload(&AgentEvent::BackgroundCompleted {
                id: 7,
                output: "ok".into(),
                label: Some("cargo".into()),
                started_at_ms: None,
                duration_ms: None,
                exit_code: None,
                signal: None,
                status: None,
                kind: None,
            }),
            json!({"id": 7, "output": "ok", "label": "cargo",
                   "started_at_ms": null, "duration_ms": null, "exit_code": null,
                   "signal": null, "status": null, "kind": null})
        );
        assert_eq!(
            event_payload(&AgentEvent::BackgroundCompletionNotice {
                id: 7,
                output: "ok".into(),
                label: None,
                started_at_ms: Some(1_700_000_000_000u64),
                duration_ms: Some(42),
                exit_code: Some(0),
                signal: None,
                status: Some("completed".into()),
                kind: Some("bash".into()),
            }),
            json!({"id": 7, "output": "ok", "label": null,
                   "started_at_ms": 1_700_000_000_000u64, "duration_ms": 42, "exit_code": 0,
                   "signal": null, "status": "completed", "kind": "bash"})
        );
        assert_eq!(
            event_payload(&AgentEvent::Usage {
                context_input: 1234,
                context_window: Some(4096),
                session: crate::agent::Usage {
                    input_tokens: 100,
                    output_tokens: 50,
                    ..Default::default()
                },
            }),
            json!({"context_input": 1234, "context_window": 4096, "session": {"input_tokens": 100, "output_tokens": 50}})
        );
    }

    /// Summary machinery: substantive counting skips streaming deltas and
    /// transient queue projections; the digest keeps the newest `max`
    /// substantive events in chronological order.
    #[test]
    fn summary_substantive_count_and_digest() {
        let events = vec![
            AgentEvent::PromptQueued("queued".into()),
            AgentEvent::UserPrompt("帮我修 bug".into()),
            AgentEvent::AssistantDelta("增量".into()),
            AgentEvent::AssistantText("我先看看".into()),
            AgentEvent::ToolCall {
                name: "bash".into(),
                arguments: "cargo build".into(),
            },
            AgentEvent::ToolResult {
                is_error: false,
                content: "ok".into(),
            },
            AgentEvent::ReasoningDelta("思考".into()),
            AgentEvent::Notice("后台任务完成".into()),
            AgentEvent::BackgroundCompletionNotice {
                id: 1,
                output: "done".into(),
                label: None,
                started_at_ms: None,
                duration_ms: None,
                exit_code: None,
                signal: None,
                status: None,
                kind: None,
            },
            AgentEvent::Usage {
                context_input: 1,
                context_window: None,
                session: crate::agent::Usage {
                    input_tokens: 1,
                    output_tokens: 0,
                    ..Default::default()
                },
            },
        ];
        // Substantive: UserPrompt, AssistantText, ToolCall, ToolResult,
        // BackgroundCompletionNotice = 5. PromptQueued/AssistantDelta/
        // ReasoningDelta/Notice/Usage are not counted.
        assert_eq!(substantive_count(&events), 5);
        let digest = digest_recent(&events, 3);
        assert_eq!(
            digest,
            "调用工具 bash: cargo build\n工具结果: ok\n后台任务完成: done"
        );
        // `max` caps from the newest side.
        let digest2 = digest_recent(&events, 2);
        assert_eq!(digest2, "工具结果: ok\n后台任务完成: done");
        // All-transient log -> empty digest (no model call).
        assert_eq!(
            digest_recent(
                &[
                    AgentEvent::PromptQueued("q".into()),
                    AgentEvent::AssistantDelta("d".into())
                ],
                20
            ),
            ""
        );
    }

    /// Summary cache + endpoint: put/get roundtrip; 404 when the session is
    /// unknown or live but not yet summarized; 200 + `{"summary","at"}`
    /// once a summary exists.
    #[tokio::test]
    async fn summary_cache_and_endpoint() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        let state = test_app_state("sekrit");
        let app = router(state.clone());

        // Unknown session -> 404 (registry miss before the cache is read).
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/sessions/nope/summary")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // Live session without a cached summary -> 404 (pet falls back).
        let (id, session) = live_session("web-abc");
        state.registry.insert(id.clone(), session.clone());
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/sessions/{id}/summary"))
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // Cached summary -> 200 with text + rfc3339 timestamp.
        summary_put(&state, &id, "正在调试 Windows 沙盒编译错误".into());
        assert!(summary_get(&state, &id).is_some());
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/sessions/{id}/summary"))
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["summary"], "正在调试 Windows 沙盒编译错误");
        // chrono's `to_rfc3339()` renders UTC as "+00:00" (not "Z").
        let at = value["at"].as_str().expect("at is a string");
        assert!(!at.is_empty() && at.contains('T') && at.ends_with("+00:00"));
    }

    #[test]
    fn prompt_body_accepts_text_and_prompt() {
        // The frontend sends `JSON.stringify({ text })`.
        let via_text: PromptBody = serde_json::from_str(r#"{"text": "hi"}"#).unwrap();
        assert_eq!(via_text.prompt, "hi");
        let via_prompt: PromptBody = serde_json::from_str(r#"{"prompt": "hi"}"#).unwrap();
        assert_eq!(via_prompt.prompt, "hi");
    }

    #[test]
    fn create_session_body_parses_initial_prompt() {
        let with_prompt: CreateSessionBody =
            serde_json::from_str(r#"{"initial_prompt": "hi"}"#).unwrap();
        assert_eq!(with_prompt.initial_prompt.as_deref(), Some("hi"));
        let empty: CreateSessionBody = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(empty.initial_prompt, None);
        let with_id: CreateSessionBody =
            serde_json::from_str(r#"{"id": "web-x", "initial_prompt": "go"}"#).unwrap();
        assert_eq!(with_id.id.as_deref(), Some("web-x"));
        assert_eq!(with_id.initial_prompt.as_deref(), Some("go"));
    }

    /// Resuming uses the persisted model for web display while the runner
    /// retains the factory's current model; newly created sessions display
    /// that current factory model.
    #[tokio::test]
    async fn resumed_session_lists_persisted_model_and_fresh_session_lists_current_model() {
        use axum::http::Request;
        use tower::util::ServiceExt;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        SessionStore::Jsonl
            .create_meta(&root, "historical", Some("X"), None, None, None, None)
            .await
            .unwrap();
        let state = Arc::new(AppState {
            factory: crate::session_factory::SessionFactory::test_factory(root),
            registry: Arc::new(SessionRegistry::default()),
            token: "sekrit".to_owned(),
            meta_store: SessionStore::Jsonl,
            summaries: Arc::new(Mutex::new(HashMap::new())),
            summary_pending: Arc::new(SummaryPending(Mutex::new(HashSet::new()))),
            shutdown: watch::channel(()).0,
        });
        let app = router(state);
        let post = |body: &'static str| {
            let app = app.clone();
            async move {
                app.oneshot(
                    Request::builder()
                        .uri("/api/sessions")
                        .method("POST")
                        .header(header::AUTHORIZATION, "Bearer sekrit")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap()
            }
        };

        let resumed = post(r#"{"id":"historical"}"#).await;
        assert_eq!(resumed.status(), StatusCode::CREATED);
        let fresh = post("{}").await;
        assert_eq!(fresh.status(), StatusCode::CREATED);
        let fresh: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(fresh.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(fresh["model"], "test-model");

        let listed = app
            .oneshot(
                Request::builder()
                    .uri("/api/sessions")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(listed.status(), StatusCode::OK);
        let listed: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(listed.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            listed
                .as_array()
                .expect("session list")
                .iter()
                .find(|meta| meta["id"] == "historical")
                .expect("resumed session is listed")["model"],
            "X"
        );
    }

    /// `POST /api/sessions {id}` — the web resume entry — must never build
    /// a second live session over a subagent id that is still live in a
    /// parent's `Sessions` registry (concurrent-write conflicts #46/#49):
    /// the parent knows its subagents, so resuming a running subagent is
    /// rejected with 409. A Busy delegate and an Idle btw subagent both
    /// block (any live handle owns the session file). A finished subagent
    /// (no handle anywhere) resumes normally; the guard must not touch it.
    #[tokio::test]
    async fn create_session_rejects_resuming_live_subagents() {
        use tower::util::ServiceExt;

        let temp = tempfile::tempdir().unwrap();
        let state = Arc::new(AppState {
            factory: crate::session_factory::SessionFactory::test_factory(
                temp.path().to_path_buf(),
            ),
            registry: Arc::new(SessionRegistry::default()),
            token: "sekrit".to_owned(),
            meta_store: SessionStore::Jsonl,
            summaries: Arc::new(Mutex::new(HashMap::new())),
            summary_pending: Arc::new(SummaryPending(Mutex::new(HashSet::new()))),
            shutdown: watch::channel(()).0,
        });
        let app = router(state.clone());

        // A live parent session in the main registry holding two live
        // subagents: one Busy (delegate-style), one Idle (btw-style).
        let (parent_id, parent) = live_session("web-parent");
        state.registry.insert(parent_id, parent.clone());
        for (task_id, (sid, status)) in [
            (1u64, ("sub-busy", SessionStatus::Busy)),
            (2u64, ("sub-idle", SessionStatus::Idle)),
        ] {
            let (handle, emitter, _commands) = crate::runner::session_test_channel();
            emitter.set_status(status);
            parent.sessions.insert(
                task_id,
                Arc::new(crate::delegate::SessionEntry {
                    handle,
                    model: "sub-model".into(),
                    role: None,
                    cwd: "/tmp".into(),
                    session_id: sid.to_owned(),
                    context_window: None,
                    store: SessionStore::Jsonl,
                }),
            );
        }

        let resume = |sid: String| {
            let app = app.clone();
            let body = format!(r#"{{"id": "{sid}"}}"#);
            async move {
                app.oneshot(
                    Request::builder()
                        .uri("/api/sessions")
                        .method("POST")
                        .header(header::AUTHORIZATION, "Bearer sekrit")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap()
            }
        };
        for (sid, expected) in [
            ("sub-busy".to_owned(), StatusCode::CONFLICT),
            ("sub-idle".to_owned(), StatusCode::CONFLICT),
            // No handle anywhere (and no running_tasks row): a finished
            // subagent resumes through to a fresh build over the id.
            ("sub-finished".to_owned(), StatusCode::CREATED),
        ] {
            let response = resume(sid.clone()).await;
            assert_eq!(response.status(), expected, "{sid}");
            if expected == StatusCode::CONFLICT {
                let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                    .await
                    .unwrap();
                let text = String::from_utf8(body.to_vec()).unwrap();
                assert!(text.contains("running subagent"), "{text}");
            }
        }
    }

    /// The cross-process half of the resume guard: a surviving
    /// `running_tasks` record (JSONL `.background.jsonl` line whose
    /// `session_id` matches) means a delegate task in another process still
    /// claims the session — resume is rejected with 409 even though no live
    /// handle exists in this process. Removing the record (task completed /
    /// zombie cleaned) restores the normal resume path.
    #[tokio::test]
    async fn create_session_rejects_resume_claimed_by_running_task_row() {
        use std::io::Write;
        use tower::util::ServiceExt;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let sessions_dir = root.join(".e-agent/sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let record_path = sessions_dir.join("web-parent.background.jsonl");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&record_path)
            .unwrap();
        // Exactly the JSONL shape `record_background_start` writes for a
        // delegate task: label + the subagent's session id.
        writeln!(
            file,
            r#"{{"id": 3, "label": "delegate work", "session_id": "sub-claimed"}}"#
        )
        .unwrap();
        file.sync_all().unwrap();

        let state = Arc::new(AppState {
            factory: crate::session_factory::SessionFactory::test_factory(root),
            registry: Arc::new(SessionRegistry::default()),
            token: "sekrit".to_owned(),
            meta_store: SessionStore::Jsonl,
            summaries: Arc::new(Mutex::new(HashMap::new())),
            summary_pending: Arc::new(SummaryPending(Mutex::new(HashSet::new()))),
            shutdown: watch::channel(()).0,
        });
        let app = router(state);

        let resume = || {
            let app = app.clone();
            async move {
                app.oneshot(
                    Request::builder()
                        .uri("/api/sessions")
                        .method("POST")
                        .header(header::AUTHORIZATION, "Bearer sekrit")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(r#"{"id": "sub-claimed"}"#.to_owned()))
                        .unwrap(),
                )
                .await
                .unwrap()
            }
        };

        let response = resume().await;
        assert_eq!(
            response.status(),
            StatusCode::CONFLICT,
            "surviving row blocks"
        );
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            text.contains("claimed by a running delegate task"),
            "{text}"
        );

        // Row consumed (task completed / zombie cleaned): same id resumes
        // normally through to a fresh build over the id.
        std::fs::remove_file(&record_path).unwrap();
        let response = resume().await;
        assert_eq!(response.status(), StatusCode::CREATED, "no row resumes");
    }

    #[test]
    fn btw_body_parses_prompt() {
        let body: BtwBody = serde_json::from_str(r#"{"prompt": "why?"}"#).unwrap();
        assert_eq!(body.prompt, "why?");
    }

    /// `POST /api/sessions/{id}/btw` rejects an empty prompt with 400 and
    /// an unknown source session with 404 — both short-circuit before any
    /// spawn work (no store I/O, no model call).
    #[tokio::test]
    async fn session_btw_rejects_empty_prompt_and_unknown_session() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        let state = test_app_state("sekrit");
        let app = router(state);
        let request = |uri: String, body: String| {
            let app = app.clone();
            async move {
                app.oneshot(
                    Request::builder()
                        .uri(uri)
                        .method("POST")
                        .header(header::AUTHORIZATION, "Bearer sekrit")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap()
            }
        };
        let empty = request(
            "/api/sessions/web-empty/btw".to_owned(),
            r#"{"prompt": "   "}"#.to_owned(),
        )
        .await;
        assert_eq!(empty.status(), StatusCode::BAD_REQUEST, "empty prompt");
        let ghost = request(
            "/api/sessions/web-ghost/btw".to_owned(),
            r#"{"prompt": "hi"}"#.to_owned(),
        )
        .await;
        assert_eq!(ghost.status(), StatusCode::NOT_FOUND, "unknown session");
    }

    /// `GET /api/models` — 200 + every switchable profile name from the
    /// factory config (`[models]` keys + `[roles]` values, deduplicated and
    /// sorted); empty array when there is no config; 401 without a token.
    #[tokio::test]
    async fn models_endpoint_lists_switchable_profiles() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        let temp = tempfile::tempdir().unwrap();
        let key = temp.path().join("key");
        std::fs::write(&key, "test-key").unwrap();
        let config: crate::config::Config = toml::from_str(&format!(
            r#"
default = "kimi/k3"
[providers.kimi]
base_url = "https://test.test/v1"
api_key_file = "{}"
[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
api_key_file = "{}"
[models."kimi/k3"]
model = "k3"
[models."deepseek/flash"]
model = "deepseek-chat"
[models."deepseek/high"]
model = "deepseek-reasoner"
[roles]
subagent = "deepseek/high"
"#,
            key.display(),
            key.display(),
        ))
        .unwrap();
        let state = Arc::new(AppState {
            factory: crate::session_factory::SessionFactory::test_factory_with_config(
                temp.path().to_path_buf(),
                Some(config),
            ),
            registry: Arc::new(SessionRegistry::default()),
            token: "sekrit".to_owned(),
            meta_store: SessionStore::Jsonl,
            summaries: Arc::new(Mutex::new(HashMap::new())),
            summary_pending: Arc::new(SummaryPending(Mutex::new(HashSet::new()))),
            shutdown: watch::channel(()).0,
        });
        let app = router(state);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/models")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let profiles: Vec<&str> = value
            .as_array()
            .expect("json array")
            .iter()
            .map(|v| v.as_str().expect("string profile"))
            .collect();
        assert_eq!(
            profiles,
            vec!["deepseek/flash", "deepseek/high", "kimi/k3"],
            "models keys + roles values, deduped and sorted"
        );

        // No config → empty list.
        let no_config_app = router(Arc::new(AppState {
            factory: crate::session_factory::SessionFactory::test_factory(std::env::temp_dir()),
            registry: Arc::new(SessionRegistry::default()),
            token: "sekrit".to_owned(),
            meta_store: SessionStore::Jsonl,
            summaries: Arc::new(Mutex::new(HashMap::new())),
            summary_pending: Arc::new(SummaryPending(Mutex::new(HashSet::new()))),
            shutdown: watch::channel(()).0,
        }));
        let empty = no_config_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/models")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(empty.status(), StatusCode::OK);
        let body = axum::body::to_bytes(empty.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"[]", "no config → empty list");

        // Auth still applies to the new endpoint.
        let unauthorized = no_config_app
            .oneshot(
                Request::builder()
                    .uri("/api/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    }

    /// `POST /api/sessions/{id}/model` — 200 + updated registry metadata for
    /// a valid profile (the live runner gets the switch via its handle; the
    /// registry display name is mirrored so `GET /api/sessions` reflects the
    /// new model), 400 for an unknown/empty profile, 404 for an unknown
    /// session.
    #[tokio::test]
    async fn session_model_switches_and_reports_unknown_profile() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        let temp = tempfile::tempdir().unwrap();
        let key = temp.path().join("key");
        std::fs::write(&key, "test-key").unwrap();
        let config: crate::config::Config = toml::from_str(&format!(
            r#"
default = "kimi/k3"
[providers.kimi]
base_url = "https://test.test/v1"
api_key_file = "{}"
[models."kimi/k3"]
model = "k3"
[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
api_key_file = "{}"
[models."deepseek/flash"]
model = "deepseek-chat"
"#,
            key.display(),
            key.display(),
        ))
        .unwrap();
        let registry = SessionRegistry::default();
        let (id, session) = live_session("web-model");
        registry.insert(id.clone(), session.clone());
        let state = Arc::new(AppState {
            factory: crate::session_factory::SessionFactory::test_factory_with_config(
                temp.path().to_path_buf(),
                Some(config),
            ),
            registry: Arc::new(registry),
            token: "sekrit".to_owned(),
            meta_store: SessionStore::Jsonl,
            summaries: Arc::new(Mutex::new(HashMap::new())),
            summary_pending: Arc::new(SummaryPending(Mutex::new(HashSet::new()))),
            shutdown: watch::channel(()).0,
        });
        let app = router(state);
        let request = |uri: String, body: String| {
            let app = app.clone();
            async move {
                app.oneshot(
                    Request::builder()
                        .uri(uri)
                        .method("POST")
                        .header(header::AUTHORIZATION, "Bearer sekrit")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap()
            }
        };

        let switched = request(
            format!("/api/sessions/{id}/model"),
            r#"{"profile": "deepseek/flash"}"#.to_owned(),
        )
        .await;
        assert_eq!(switched.status(), StatusCode::OK);
        let body = axum::body::to_bytes(switched.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["model"], "deepseek/flash", "full profile key");
        // The registry metadata mirrors the switch for `GET /api/sessions`.
        assert_eq!(session.model_name(), "deepseek/flash");

        let unknown = request(
            format!("/api/sessions/{id}/model"),
            r#"{"profile": "nope/missing"}"#.to_owned(),
        )
        .await;
        assert_eq!(unknown.status(), StatusCode::BAD_REQUEST, "unknown profile");

        let empty = request(
            format!("/api/sessions/{id}/model"),
            r#"{"profile": "   "}"#.to_owned(),
        )
        .await;
        assert_eq!(empty.status(), StatusCode::BAD_REQUEST, "empty profile");

        let ghost = request(
            "/api/sessions/web-ghost/model".to_owned(),
            r#"{"profile": "deepseek/flash"}"#.to_owned(),
        )
        .await;
        assert_eq!(ghost.status(), StatusCode::NOT_FOUND, "unknown session");
    }

    /// `POST /api/sessions/{id}/undo` — 404 for an unknown session, 409
    /// with a Chinese error when the stack is empty, and 200 +
    /// `{"ok": true, "message": ...}` that actually reverts the file when a
    /// write_file snapshot exists.
    #[tokio::test]
    async fn session_undo_endpoint_reverts_last_file_op() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        // Serialize with the backend undo tests: the undo stack is global.
        let _guard = crate::tools::UNDO_TEST_LOCK.lock().await;
        crate::tools::clear_undo_stack();

        let state = test_app_state("sekrit");
        let app = router(state.clone());

        // Unknown session -> 404 (registry miss, like its siblings).
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/sessions/nope/undo")
                    .method("POST")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // Live session but nothing recorded -> 409 with a clear message.
        let (id, session) = live_session("web-undo");
        state.registry.insert(id.clone(), session.clone());
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/sessions/{id}/undo"))
                    .method("POST")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("没有可撤销"), "{text}");

        // Record a write_file snapshot on a temp workspace, then undo via
        // the endpoint: 200 + ok:true + the file reverted.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file.txt");
        std::fs::write(&path, "old").unwrap();
        let workspace = crate::workspace::Workspace::new(temp.path()).unwrap();
        let tools = crate::tools::file_tools(&workspace);
        let write = tools
            .iter()
            .find(|tool| tool.spec().name == "write_file")
            .expect("file_tools includes write_file");
        write
            .execute(serde_json::json!({"path": "file.txt", "content": "new"}))
            .await
            .unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/sessions/{id}/undo"))
                    .method("POST")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["ok"], true);
        let message = value["message"].as_str().unwrap();
        assert!(message.contains("已撤销 write_file: file.txt"), "{message}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
    }

    /// Goal endpoints on a REAL runner (WaitForInput, no prompt — the
    /// runner never calls the model): GET returns the committed snapshot,
    /// POST set creates revision 1, a second set is pre-validated 409,
    /// pause/resume transition the status, clear tombstones to null, and
    /// the runner persists each change to the session store.
    #[tokio::test]
    async fn goal_endpoints_create_read_actions_and_clear() {
        use async_trait::async_trait;
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        use crate::agent::Agent;
        use crate::runner::SessionRunner;

        /// Holds the agent's background-completion sender open so the idle
        /// WaitForInput runner does not finalize Closed between commands.
        struct HoldSender(Option<tokio::sync::mpsc::UnboundedSender<crate::agent::AgentEvent>>);
        #[async_trait]
        impl crate::agent::Tool for HoldSender {
            fn spec(&self) -> crate::agent::ToolSpec {
                crate::agent::ToolSpec {
                    name: "hold_sender".into(),
                    description: "test".into(),
                    parameters: serde_json::json!({}),
                }
            }
            async fn execute(
                &self,
                _: serde_json::Value,
            ) -> Result<crate::agent::ToolOutput, String> {
                Err("unused".into())
            }
            fn set_event_sender(
                &mut self,
                sender: tokio::sync::mpsc::UnboundedSender<crate::agent::AgentEvent>,
            ) {
                self.0 = Some(sender);
            }
        }

        struct NeverCalledModel;
        #[async_trait]
        impl crate::agent::Model for NeverCalledModel {
            async fn complete(
                &mut self,
                _: &[crate::agent::Message],
                _: &[crate::agent::ToolSpec],
                _: Option<&mut (dyn for<'a> FnMut(crate::agent::ModelDeltaKind, &'a str) + Send)>,
            ) -> anyhow::Result<(crate::agent::AssistantMessage, Option<crate::agent::Usage>)>
            {
                panic!("goal endpoint test must never call the model");
            }
        }

        let state = test_app_state("sekrit");
        let temp = tempfile::tempdir().unwrap();
        let (runner, handle) = SessionRunner::new(
            Agent::new(Box::new(NeverCalledModel), vec![Box::new(HoldSender(None))]),
            SessionStore::Jsonl,
            temp.path().into(),
            "web-goal".into(),
            IdlePolicy::WaitForInput,
        );
        let runner_task = runner.start(None);
        let workspace = crate::workspace::Workspace::new(std::env::temp_dir()).unwrap();
        let (_tools, background) = crate::tools::builtins(workspace, None, false, None);
        state.registry.insert(
            "web-goal".into(),
            Arc::new(LiveSession {
                handle: handle.clone(),
                task: runner_task,
                store: SessionStore::Jsonl,
                background,
                sessions: Sessions::default(),
                model_name: Mutex::new("test-model".into()),
                role_name: None,
                created_at: chrono::Utc::now(),
            }),
        );
        let app = router(state.clone());
        let get_goal = |app: Router| async move {
            let response = app
                .oneshot(
                    Request::builder()
                        .uri("/api/sessions/web-goal/goal")
                        .header(header::AUTHORIZATION, "Bearer sekrit")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
                .await
                .unwrap();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
        };
        let post_goal = |app: Router, body: String| async move {
            app.oneshot(
                Request::builder()
                    .uri("/api/sessions/web-goal/goal")
                    .method("POST")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
        };
        async fn wait_goal(
            handle: &SessionHandle,
            expected: impl Fn(Option<&crate::agent::GoalSnapshot>) -> bool,
        ) {
            for _ in 0..400 {
                if expected(handle.goal().as_ref()) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            panic!("goal condition not met in time");
        }

        // Fresh session: GET → null.
        assert!(get_goal(app.clone()).await["goal"].is_null());

        // set → 202; the runner applies asynchronously and GET reflects it.
        assert_eq!(
            post_goal(
                app.clone(),
                r#"{"action":"set","objective":"ship it","success_criteria":["tests pass"]}"#
                    .to_owned()
            )
            .await,
            StatusCode::ACCEPTED
        );
        wait_goal(&handle, |g| g.is_some()).await;
        let goal = handle.goal().unwrap();
        assert_eq!(goal.status, crate::agent::GoalStatus::Active);
        assert_eq!(goal.revision, 1);
        let json = get_goal(app.clone()).await;
        assert_eq!(json["goal"]["objective"], "ship it");
        assert_eq!(json["goal"]["success_criteria"][0], "tests pass");

        // set again while active → 409 (pre-validated synchronously).
        assert_eq!(
            post_goal(
                app.clone(),
                r#"{"action":"set","objective":"second"}"#.to_owned()
            )
            .await,
            StatusCode::CONFLICT
        );
        // pause → 202 → paused; resume → 202 → active; clear → 202 → null.
        assert_eq!(
            post_goal(app.clone(), r#"{"action":"pause"}"#.to_owned()).await,
            StatusCode::ACCEPTED
        );
        wait_goal(&handle, |g| {
            g.map(|g| g.status) == Some(crate::agent::GoalStatus::Paused)
        })
        .await;
        assert_eq!(
            post_goal(app.clone(), r#"{"action":"resume"}"#.to_owned()).await,
            StatusCode::ACCEPTED
        );
        wait_goal(&handle, |g| {
            g.map(|g| g.status) == Some(crate::agent::GoalStatus::Active)
        })
        .await;
        // Unknown action → 400.
        assert_eq!(
            post_goal(app.clone(), r#"{"action":"skip"}"#.to_owned()).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            post_goal(app.clone(), r#"{"action":"clear"}"#.to_owned()).await,
            StatusCode::ACCEPTED
        );
        wait_goal(&handle, |g| g.is_none()).await;
        assert!(get_goal(app.clone()).await["goal"].is_null());

        // Persistence: create + pause + resume + clear all appended.
        let loaded = SessionStore::Jsonl
            .load(temp.path(), "web-goal")
            .await
            .unwrap();
        let updates = loaded
            .entries
            .iter()
            .filter(|entry| matches!(entry, SessionEntry::GoalUpdated { .. }))
            .count();
        assert_eq!(updates, 4);
    }

    #[tokio::test]
    async fn goal_post_rejects_unknown_fields_with_400_and_applies_nothing() {
        // `deny_unknown_fields` on GoalBody: a misspelled `success_criteria`
        // or any arbitrary extra key is a synchronous 400 from the JSON
        // extractor — the handler never runs, so nothing is queued or
        // applied (no goal, no GoalUpdated entry).
        use async_trait::async_trait;
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        use crate::agent::Agent;
        use crate::runner::SessionRunner;

        /// Holds the agent's background-completion sender open so the idle
        /// WaitForInput runner does not finalize Closed between commands.
        struct HoldSender(Option<tokio::sync::mpsc::UnboundedSender<crate::agent::AgentEvent>>);
        #[async_trait]
        impl crate::agent::Tool for HoldSender {
            fn spec(&self) -> crate::agent::ToolSpec {
                crate::agent::ToolSpec {
                    name: "hold_sender".into(),
                    description: "test".into(),
                    parameters: serde_json::json!({}),
                }
            }
            async fn execute(
                &self,
                _: serde_json::Value,
            ) -> Result<crate::agent::ToolOutput, String> {
                Err("unused".into())
            }
            fn set_event_sender(
                &mut self,
                sender: tokio::sync::mpsc::UnboundedSender<crate::agent::AgentEvent>,
            ) {
                self.0 = Some(sender);
            }
        }

        struct NeverCalledModel;
        #[async_trait]
        impl crate::agent::Model for NeverCalledModel {
            async fn complete(
                &mut self,
                _: &[crate::agent::Message],
                _: &[crate::agent::ToolSpec],
                _: Option<&mut (dyn for<'a> FnMut(crate::agent::ModelDeltaKind, &'a str) + Send)>,
            ) -> anyhow::Result<(crate::agent::AssistantMessage, Option<crate::agent::Usage>)>
            {
                panic!("goal endpoint test must never call the model");
            }
        }

        let state = test_app_state("sekrit");
        let temp = tempfile::tempdir().unwrap();
        let (runner, handle) = SessionRunner::new(
            Agent::new(Box::new(NeverCalledModel), vec![Box::new(HoldSender(None))]),
            SessionStore::Jsonl,
            temp.path().into(),
            "web-goal-strict".into(),
            IdlePolicy::WaitForInput,
        );
        let runner_task = runner.start(None);
        let workspace = crate::workspace::Workspace::new(std::env::temp_dir()).unwrap();
        let (_tools, background) = crate::tools::builtins(workspace, None, false, None);
        state.registry.insert(
            "web-goal-strict".into(),
            Arc::new(LiveSession {
                handle: handle.clone(),
                task: runner_task,
                store: SessionStore::Jsonl,
                background,
                sessions: Sessions::default(),
                model_name: Mutex::new("test-model".into()),
                role_name: None,
                created_at: chrono::Utc::now(),
            }),
        );
        let app = router(state.clone());
        let post_goal = |app: Router, body: String| async move {
            let response = app
                .oneshot(
                    Request::builder()
                        .uri("/api/sessions/web-goal-strict/goal")
                        .method("POST")
                        .header(header::AUTHORIZATION, "Bearer sekrit")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap();
            (status, String::from_utf8_lossy(&bytes).into_owned())
        };
        // Misspelled field and arbitrary extra keys: both 400, naming the
        // offending field in the reject body.
        for body in [
            r#"{"action":"set","objective":"nope","sucess_criteria":["typo"]}"#,
            r#"{"action":"set","objective":"nope","extra":"bogus"}"#,
            r#"{"action":"set","objective":"nope","success_criteria":["ok"],"bogus":1}"#,
        ] {
            let (status, text) = post_goal(app.clone(), body.to_owned()).await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "unknown POST /goal fields must 400 on {body}"
            );
            assert!(
                text.contains("unknown field") || text.contains("deny_unknown_fields"),
                "400 body should explain the unknown field: {text}"
            );
        }
        // Nothing was queued or applied: no goal, GET stays null, and the
        // persisted log has no GoalUpdated entry.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            handle.goal().is_none(),
            "rejected body must not apply a goal"
        );
        let get = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/sessions/web-goal-strict/goal")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(get.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(get.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json["goal"].is_null());
        let loaded = SessionStore::Jsonl
            .load(temp.path(), "web-goal-strict")
            .await
            .unwrap();
        assert!(
            !loaded
                .entries
                .iter()
                .any(|entry| matches!(entry, SessionEntry::GoalUpdated { .. })),
            "rejected bodies must never enqueue a GoalUpdated entry"
        );
    }

    #[tokio::test]
    async fn goal_post_rejects_finished_session_with_409() {
        use async_trait::async_trait;
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        use crate::agent::Agent;
        use crate::runner::SessionRunner;

        /// Finishes on its first call: the runner then finalizes and closes
        /// its command channel (Finished status, commands_open=false).
        struct FinishingModel;
        #[async_trait]
        impl crate::agent::Model for FinishingModel {
            async fn complete(
                &mut self,
                _: &[crate::agent::Message],
                _: &[crate::agent::ToolSpec],
                _: Option<&mut (dyn for<'a> FnMut(crate::agent::ModelDeltaKind, &'a str) + Send)>,
            ) -> anyhow::Result<(crate::agent::AssistantMessage, Option<crate::agent::Usage>)>
            {
                Ok((
                    crate::agent::AssistantMessage {
                        content: Some("done".into()),
                        tool_calls: Vec::new(),
                        reasoning: None,
                    },
                    None,
                ))
            }
        }

        let state = test_app_state("sekrit");
        let temp = tempfile::tempdir().unwrap();
        let (runner, handle) = SessionRunner::new(
            Agent::new(Box::new(FinishingModel), Vec::new()),
            SessionStore::Jsonl,
            temp.path().into(),
            "web-goal-finished".into(),
            IdlePolicy::FinishWhenIdle,
        );
        let task = runner.start(Some("run once".into()));
        let mut status = handle.status();
        loop {
            if matches!(&*status.borrow(), SessionStatus::Finished(_)) {
                break;
            }
            status.changed().await.unwrap();
        }
        let workspace = crate::workspace::Workspace::new(std::env::temp_dir()).unwrap();
        let (_tools, background) = crate::tools::builtins(workspace, None, false, None);
        state.registry.insert(
            "web-goal-finished".into(),
            Arc::new(LiveSession {
                handle: handle.clone(),
                task,
                store: SessionStore::Jsonl,
                background,
                sessions: Sessions::default(),
                model_name: Mutex::new("test-model".into()),
                role_name: None,
                created_at: chrono::Utc::now(),
            }),
        );
        let app = router(state.clone());

        // GET stays readable (read-only), POST is a hard 409 — never a
        // hollow 202 that the closed runner would silently drop.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/sessions/web-goal-finished/goal")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        for body in [
            r#"{"action":"set","objective":"nope"}"#,
            r#"{"action":"pause"}"#,
            r#"{"action":"clear"}"#,
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/api/sessions/web-goal-finished/goal")
                        .method("POST")
                        .header(header::AUTHORIZATION, "Bearer sekrit")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::CONFLICT,
                "finished session must 409 on {body}"
            );
        }
    }

    #[tokio::test]
    async fn goal_post_rejects_closed_command_channel_with_409() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        // Status 非 Finished（Idle）但 command channel 已 closed（receiver
        // 被 drop，模拟 runner 任务已退出但状态尚未刷新）：POST 必须 409 ——
        // 走的是 goal_command 的 closed-channel 回退，不是 Finished 前置检查。
        let state = test_app_state("sekrit");
        let (handle, _emitter, receiver) = crate::runner::session_test_channel();
        drop(receiver);
        assert_eq!(
            &*handle.status().borrow(),
            &SessionStatus::Idle,
            "precondition: status Idle, not Finished"
        );
        assert!(
            !handle.goal_command(crate::runner::GoalCommand::Create {
                objective: "nope".into(),
                success_criteria: vec![],
            }),
            "closed channel must reject the command"
        );
        state
            .registry
            .insert("web-goal-closed".into(), live_session_with_handle(handle));
        let app = router(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/sessions/web-goal-closed/goal")
                    .method("POST")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"action":"set","objective":"nope"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::CONFLICT,
            "closed command channel must 409, never a hollow 202"
        );
    }

    #[test]
    #[cfg(not(web_ui))]
    fn placeholder_contains_running_and_token() {
        let html = PLACEHOLDER_HTML.replace("__TOKEN__", "abc");
        assert!(html.contains("server running"));
        assert!(html.contains("abc"));
    }

    /// The release UI path (include_str! table) must assemble exactly like
    /// the dev disk path: all ten assets present, all five placeholders
    /// replaced, and the key frontend entry points inlined.
    #[cfg(web_ui)]
    #[test]
    fn embedded_ui_assembles_all_assets() {
        let html = assemble(read_embedded_ui).unwrap();
        for placeholder in [
            "/*__KATEX_CSS__*/",
            "/*__CSS__*/",
            "/*__JS_VENDOR__*/",
            "/*__JS_APP__*/",
            "<!--__PET__-->",
        ] {
            assert!(
                !html.contains(placeholder),
                "placeholder {placeholder} leaked into assembled UI"
            );
        }
        assert!(html.contains("<title>e-agent · Web UI</title>"));
        assert!(html.contains("marked.setOptions"));
        let pet = read_embedded_ui("pet.html").unwrap();
        for artwork in [
            "<svg",
            "maid-silhouette",
            "maid-hair-back",
            "maid-headpiece",
            "maid-face",
            "maid-uniform",
            "maid-accents",
            ".pet-whale-svg",
            ".pet-body",
        ] {
            assert!(!pet.contains(artwork), "pet artwork leaked: {artwork}");
        }
        assert!(pet.contains("class=\"pet-whale\"") && pet.contains("hidden>"));
        assert!(pet.contains("fish.hidden = false"));
        assert!(
            !pet.contains("max-width: 480px"),
            "configured pet must remain visible on narrow screens"
        );
        assert!(html.contains("katex.renderToString"));
        // KaTeX fonts are inlined as base64 data: URLs (self-contained,
        // zero external requests): every @font-face src must be a woff2
        // data URL and no relative `fonts/` fetch may survive.
        assert!(
            !html.contains("url(fonts/"),
            "relative KaTeX font URLs leaked into assembled UI"
        );
        assert!(
            html.contains("data:font/woff2;base64,"),
            "no inlined KaTeX woff2 data URL in assembled UI"
        );
        assert_eq!(
            html.matches("data:font/woff2;base64,").count(),
            20,
            "expected exactly 20 inlined KaTeX font faces"
        );
    }

    /// GET / serves the assembled UI uncached: 200, text/html, no-store,
    /// and no placeholder markers in the body. Runs against the dev disk
    /// reader in debug and the embedded reader in release.
    #[cfg(web_ui)]
    #[tokio::test]
    async fn index_serves_assembled_ui_uncached() {
        use tower::util::ServiceExt;
        let app = router(Arc::new(AppState {
            factory: crate::session_factory::SessionFactory::test_factory(std::env::temp_dir()),
            registry: Arc::new(SessionRegistry::default()),
            token: "sekrit".to_owned(),
            meta_store: SessionStore::Jsonl,
            summaries: Arc::new(Mutex::new(HashMap::new())),
            summary_pending: Arc::new(SummaryPending(Mutex::new(HashSet::new()))),
            shutdown: watch::channel(()).0,
        }));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        let body = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        for placeholder in [
            "/*__KATEX_CSS__*/",
            "/*__CSS__*/",
            "/*__JS_VENDOR__*/",
            "/*__JS_APP__*/",
            "<!--__PET__-->",
        ] {
            assert!(
                !html.contains(placeholder),
                "placeholder {placeholder} leaked into GET / body"
            );
        }
    }

    fn live_session(id: &str) -> (String, Arc<LiveSession>) {
        let (handle, _emitter, _commands) = crate::runner::session_test_channel();
        (id.to_owned(), live_session_with_handle(handle))
    }

    /// A `LiveSession` sharing the given test handle (the caller keeps the
    /// emitter so it can push events into the broadcast channel).
    fn live_session_with_handle(handle: crate::runner::SessionHandle) -> Arc<LiveSession> {
        let workspace = crate::workspace::Workspace::new(std::env::temp_dir()).unwrap();
        let (_tools, background) = crate::tools::builtins(workspace, None, false, None);
        Arc::new(LiveSession {
            handle,
            task: crate::runner::SessionTask::from_join_handle(tokio::spawn(async {})),
            store: SessionStore::Jsonl,
            background,
            sessions: Sessions::default(),
            model_name: Mutex::new("test-model".into()),
            role_name: None,
            created_at: chrono::Utc::now(),
        })
    }

    /// A `LiveSession` whose `BackgroundTasks` has a live completion sender.
    /// `builtins` alone yields a sender-less registry whose `start` rejects
    /// with "background task delivery is unavailable"; this helper wires an
    /// unbounded channel so tests can spawn real background tasks. The
    /// returned receiver keeps the channel open for the caller's scope
    /// (dropping every receiver would make `sender.is_closed()` true).
    fn live_session_with_background_sender(
        id: &str,
    ) -> (
        String,
        Arc<LiveSession>,
        tokio::sync::mpsc::UnboundedReceiver<crate::agent::AgentEvent>,
    ) {
        let (handle, _emitter, _commands) = crate::runner::session_test_channel();
        let workspace = crate::workspace::Workspace::new(std::env::temp_dir()).unwrap();
        let (_tools, mut background) = crate::tools::builtins(workspace, None, false, None);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        background.set_event_sender(tx);
        (
            id.to_owned(),
            Arc::new(LiveSession {
                handle,
                task: crate::runner::SessionTask::from_join_handle(tokio::spawn(async {})),
                store: SessionStore::Jsonl,
                background,
                sessions: Sessions::default(),
                model_name: Mutex::new("test-model".into()),
                role_name: None,
                created_at: chrono::Utc::now(),
            }),
            rx,
        )
    }

    /// An `AppState` for handler tests. `test_factory` skips config/model env
    /// resolution; the tested paths (auth middleware, registry-miss 404)
    /// never reach `factory.build()`. The meta store is the Jsonl backend;
    /// listing scans `<root>/.e-agent/sessions` (empty for a fresh temp dir).
    fn test_app_state(token: &str) -> Arc<AppState> {
        Arc::new(AppState {
            factory: crate::session_factory::SessionFactory::test_factory(std::env::temp_dir()),
            registry: Arc::new(SessionRegistry::default()),
            token: token.to_owned(),
            meta_store: SessionStore::Jsonl,
            summaries: Arc::new(Mutex::new(HashMap::new())),
            summary_pending: Arc::new(SummaryPending(Mutex::new(HashSet::new()))),
            shutdown: watch::channel(()).0,
        })
    }

    /// Serialize one SSE `Event` to its wire text (via a one-event `Sse`
    /// stream) so tests can assert the actual frame shape the frontend
    /// parses (`event: NAME` + `data: ...`).
    async fn event_to_text(event: Event) -> String {
        let sse = Sse::new(futures_util::stream::once(async move {
            Ok::<Event, Error>(event)
        }));
        let body = sse.into_response().into_body();
        let bytes = axum::body::to_bytes(body, 16 * 1024 * 1024)
            .await
            .expect("event frame fits");
        String::from_utf8(bytes.to_vec()).expect("event frame is utf-8")
    }

    #[test]
    fn registry_reservation_is_scoped_and_invisible_until_published() {
        let registry = Arc::new(SessionRegistry::default());
        let reservation = registry.reserve("creating").unwrap();
        assert!(registry.list().is_empty());
        assert!(registry.get("creating").is_none());
        assert!(registry.reserve("creating").is_err());
        drop(reservation);
        assert!(registry.reserve("creating").is_ok());
    }

    #[test]
    fn registry_reservation_allows_only_one_builder() {
        let registry = Arc::new(SessionRegistry::default());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let other = registry.clone();
        let gate = barrier.clone();
        let thread = std::thread::spawn(move || {
            let reservation = other.reserve("barrier").unwrap();
            gate.wait();
            reservation
        });
        barrier.wait();
        assert!(registry.reserve("barrier").is_err());
        drop(thread.join().unwrap());
        assert!(registry.reserve("barrier").is_ok());
    }

    #[tokio::test]
    async fn registry_insert_get_list_remove() {
        let registry = SessionRegistry::default();
        assert!(registry.list().is_empty());
        let (id, session) = live_session("web-abc");
        registry.insert(id.clone(), session.clone());
        assert_eq!(registry.get(&id).unwrap().model_name(), "test-model");
        assert_eq!(registry.list().len(), 1);
        assert!(registry.list()[0].0 == id);
        let removed = registry.remove(&id);
        assert!(removed.is_some());
        assert!(registry.get(&id).is_none());
        assert!(registry.list().is_empty());
    }

    #[test]
    fn sse_dto_helpers_build_frames() {
        // Frame construction must succeed; the wire JSON shapes are covered
        // by `status_json_is_object_with_camel_case_status`,
        // `event_name_is_camel_case_variant` and
        // `event_payload_is_flat_for_frontend` (axum 0.8's `Event` exposes
        // no getters to assert the buffer on).
        assert!(snapshot_event(&[AgentEvent::Notice("hi".into())]).is_ok());
        assert!(resync_event(&[AgentEvent::Notice("hi".into())]).is_ok());
        assert!(live_event(&AgentEvent::AssistantText("x".into())).is_ok());
        assert!(
            live_event(&AgentEvent::ToolCall {
                name: "bash".into(),
                arguments: "ls".into()
            })
            .is_ok()
        );
        assert!(status_event(&SessionStatus::Busy).is_ok());
    }

    /// Frontend contract: `GET /api/sessions` items carry `status` as a
    /// CamelCase string plus `entry_count`, `busy`, `active` and
    /// `parent_session_id` (index.html reads `s.id`, `s.status`, `s.model`,
    /// `s.created_at`, `s.entry_count`, `s.busy`, `s.active`,
    /// `s.parent_session_id`).
    #[tokio::test]
    async fn session_meta_has_frontend_fields() {
        let (id, session) = live_session("web-abc");
        let meta = session_meta(&id, &session, &std::env::temp_dir()).await;
        let value = serde_json::to_value(&meta).unwrap();
        for key in [
            "id",
            "model",
            "role",
            "created_at",
            "last_active_at",
            "status",
            "entry_count",
            "busy",
            "active",
            "parent_session_id",
            "title",
            "pinned",
            "archived",
            "label",
        ] {
            assert!(value.get(key).is_some(), "missing field {key}");
        }
        assert_eq!(value["status"], "Idle");
        assert_eq!(value["busy"], false);
        assert_eq!(value["active"], true, "registry sessions are active");
        assert_eq!(value["entry_count"], 0);
    }

    /// The list merge: registry sessions stay active (and win over an
    /// overlapping history row), historical metadata-table sessions come in
    /// inactive and resumable ("Idle").
    #[test]
    fn merge_session_metas_marks_registry_active_and_keeps_history() {
        use crate::session_store::SessionMeta as HistoryMeta;
        let dt = |secs: i64| chrono::DateTime::from_timestamp(secs, 0).unwrap();
        let naive = |secs: i64| {
            chrono::DateTime::from_timestamp(secs, 0)
                .unwrap()
                .naive_utc()
        };
        let wire = |id: &str, created: i64| SessionMeta {
            id: id.to_owned(),
            model: "web-model".into(),
            role: None,
            created_at: dt(created),
            last_active_at: dt(created),
            status: "Idle".into(),
            entry_count: 1,
            busy: false,
            active: false,
            parent_session_id: None,
            title: None,
            pinned: None,
            archived: None,
            label: None,
        };
        let history = |id: &str, created: i64, count: i64, title: Option<&str>| HistoryMeta {
            session_id: id.to_owned(),
            created_at: naive(created),
            last_active_at: naive(created + 10),
            model: None,
            role: None,
            entry_count: count,
            parent_session_id: None,
            parent_task_id: None,
            title: title.map(str::to_owned),
            pinned: None,
            archived: None,
            writer: None,
            label: None,
        };
        // One registry session plus two historical sessions, one of which
        // overlaps a registry id (the registry entry wins; the renamed
        // history row backfills the live entry's title — the registry DTO
        // has none).
        let merged = merge_session_metas(
            vec![wire("web-1", 200)],
            vec![
                history("tui-1", 50, 7, None),
                history("web-1", 100, 3, Some("renamed")),
            ],
        );
        assert_eq!(merged.len(), 2);
        let web1 = merged.iter().find(|m| m.id == "web-1").unwrap();
        assert!(web1.active, "registry session stays active");
        assert_eq!(web1.entry_count, 1, "registry data wins over history");
        assert_eq!(web1.created_at, dt(200));
        assert_eq!(
            web1.title.as_deref(),
            Some("renamed"),
            "live session title backfilled from the metadata table"
        );
        assert_eq!(
            web1.pinned, None,
            "unpinned history leaves the live entry unpinned"
        );
        let tui1 = merged.iter().find(|m| m.id == "tui-1").unwrap();
        assert!(!tui1.active, "historical session is inactive");
        assert_eq!(tui1.entry_count, 7);
        assert_eq!(tui1.status, "Idle", "historical sessions are resumable");
        assert_eq!(tui1.model, "", "history without model renders empty");
        assert_eq!(tui1.title, None, "history without title renders unnamed");
        // Newest activity first: the merged list is sorted by last_active_at
        // descending (registry web-1 has last_active=200, history tui-1 has
        // last_active=60), not by created_at.
        let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["web-1", "tui-1"],
            "most recently active sorts first"
        );

        // Pinned sorting: a pinned session (even an older one) moves to the
        // front of the list; the pin backfills a live entry too.
        let mut pinned_history = history("tui-1", 50, 7, None);
        pinned_history.pinned = Some(true);
        let merged = merge_session_metas(vec![wire("web-1", 200)], vec![pinned_history]);
        let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["tui-1", "web-1"],
            "pinned sessions sort first, newest activity second"
        );
        let tui1 = merged.iter().find(|m| m.id == "tui-1").unwrap();
        assert_eq!(tui1.pinned, Some(true), "historical pin passes through");

        // Archived sorting: an archived session (even a recent one) moves
        // to the back of the list; pinned still wins over both, and the
        // archive flag backfills a live entry too.
        let mut archived_web = history("web-1", 200, 3, None);
        archived_web.archived = Some(true);
        let mut archived_pinned = history("tui-1", 50, 7, None);
        archived_pinned.pinned = Some(true);
        archived_pinned.archived = Some(true);
        let merged = merge_session_metas(
            vec![wire("web-1", 200)],
            vec![archived_pinned, archived_web],
        );
        let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["tui-1", "web-1"],
            "pinned first (even when archived), archived sorts last"
        );
        let web1 = merged.iter().find(|m| m.id == "web-1").unwrap();
        assert_eq!(
            web1.archived,
            Some(true),
            "historical archive passes through to the live entry"
        );
    }

    /// Archive sort DIRECTION: unarchived sessions sort BEFORE archived
    /// ones, even when the archived session is more recently active — the
    /// comparator ascends on `archived` (unlike `pinned`, which descends),
    /// so archived sessions sink to the bottom of the default list.
    #[test]
    fn merge_session_metas_archived_sorts_after_unarchived() {
        use crate::session_store::SessionMeta as HistoryMeta;
        let dt = |secs: i64| chrono::DateTime::from_timestamp(secs, 0).unwrap();
        let naive = |secs: i64| {
            chrono::DateTime::from_timestamp(secs, 0)
                .unwrap()
                .naive_utc()
        };
        let wire = |id: &str, created: i64| SessionMeta {
            id: id.to_owned(),
            model: "web-model".into(),
            role: None,
            created_at: dt(created),
            last_active_at: dt(created),
            status: "Idle".into(),
            entry_count: 1,
            busy: false,
            active: false,
            parent_session_id: None,
            title: None,
            pinned: None,
            archived: None,
            label: None,
        };
        let history = |id: &str, created: i64, count: i64, archived: Option<bool>| HistoryMeta {
            session_id: id.to_owned(),
            created_at: naive(created),
            last_active_at: naive(created + 10),
            model: None,
            role: None,
            entry_count: count,
            parent_session_id: None,
            parent_task_id: None,
            title: None,
            pinned: None,
            archived,
            writer: None,
            label: None,
        };
        // The archived session is MORE recently active (last_active 510 vs
        // 110); it must still sort AFTER the unarchived one — recency only
        // breaks ties within the same archived bucket.
        let archived_recent = history("arch-1", 500, 3, Some(true));
        let unarchived_old = history("plain", 100, 1, None);
        let merged = merge_session_metas(vec![], vec![archived_recent, unarchived_old]);
        let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["plain", "arch-1"],
            "unarchived sorts before archived regardless of recency"
        );
        assert_eq!(merged[0].archived, None);
        assert_eq!(merged[1].archived, Some(true));
        // Same recency, different flags: still unarchived first.
        let merged = merge_session_metas(
            vec![],
            vec![
                history("arch-2", 300, 1, Some(true)),
                history("plain-2", 300, 1, None),
            ],
        );
        let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["plain-2", "arch-2"]);
        // Both archived: recency breaks the tie (newest first).
        let merged = merge_session_metas(
            vec![],
            vec![
                history("arch-old", 100, 1, Some(true)),
                history("arch-new", 500, 1, Some(true)),
            ],
        );
        let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["arch-new", "arch-old"],
            "recency sorts within archived"
        );
        // A live registry session is never archived by default; it must
        // beat the archived history session too.
        let merged = merge_session_metas(
            vec![wire("live-1", 900)],
            vec![history("arch-live", 800, 3, Some(true))],
        );
        let ids: Vec<&str> = merged.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["live-1", "arch-live"]);
    }

    /// The label rule for subagent list entries: `apply_subagent_label` is
    /// display-only — it never touches `active`. Liveness comes from the
    /// main registry or a live parent's `Sessions` registry (real
    /// handles), never from a surviving `running_tasks` label row alone
    /// (the label is task metadata with an async write/cleanup window, not
    /// runtime liveness).
    #[test]
    fn subagent_label_is_display_only() {
        let dt = |secs: i64| chrono::DateTime::from_timestamp(secs, 0).unwrap();
        let meta = |active: bool| SessionMeta {
            id: "sub-1".into(),
            model: "m".into(),
            role: None,
            created_at: dt(0),
            last_active_at: dt(0),
            status: "Idle".into(),
            entry_count: 1,
            busy: false,
            active,
            parent_session_id: Some("web-1".into()),
            title: None,
            pinned: None,
            archived: None,
            label: None,
        };
        let mut live = meta(false);
        apply_subagent_label(&mut live, Some("delegate task".into()));
        assert!(
            !live.active,
            "a surviving label alone never marks a subagent live"
        );
        assert_eq!(live.label.as_deref(), Some("delegate task"));
        let mut done = meta(true);
        apply_subagent_label(&mut done, None);
        assert!(
            done.active,
            "the label lookup must not clear a real liveness flag either"
        );
        assert_eq!(done.label, None);
    }

    /// `list_sessions` subagent liveness matrix (oracle#95): `active` and
    /// `busy`/`status` come only from real handles — the main registry or
    /// a live parent's `Sessions` registry — never from the task-panel
    /// label. Eight combinations:
    ///   1. Child Busy                    → active=true, status=Busy, busy=true
    ///   2. Child Compacting              → active=true, status=Compacting, busy=true
    ///   3. Child Idle + label            → active=true, busy=false, label=Some
    ///   4. Child Finished, cleanup not done → active=true, busy=false
    ///   5. stale label, no child handle  → active=false, busy=false, label=Some
    ///   6. label lookup fails/misses but child Busy → active=true, busy=true
    ///   7. parent-linked in main registry Busy, no child-map entry → busy not
    ///      overwritten to false
    ///   8. two handles for one id, one Busy one Idle → Busy wins
    ///
    /// The Jsonl backend's `label_for_subagent` never returns `Err` (a
    /// missing record is `Ok(None)`), so combination 6 stands in for the
    /// lookup-failure path: both leave the labels map without the id, and
    /// the handle-based backfill must proceed regardless.
    #[tokio::test]
    async fn list_sessions_subagent_liveness_matrix() {
        use crate::runner::SessionStatus as Status;
        use crate::session_store::SessionMeta as HistoryMeta;
        use std::collections::HashMap;
        use std::io::Write;

        let root = std::env::temp_dir().join(format!(
            "e-agent-server-subagent-liveness-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let sessions_dir = root.join(".e-agent/sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let naive = |secs: i64| {
            chrono::DateTime::from_timestamp(secs, 0)
                .unwrap()
                .naive_utc()
        };
        // Historical sidecar: every subagent under test appears in the
        // metadata table (parent-linked, Idle/inactive like any history
        // row) so the merge produces a subagent list entry for it.
        let sidecar = |id: &str, parent: &str| HistoryMeta {
            session_id: id.to_owned(),
            created_at: naive(1_700_000_000),
            last_active_at: naive(1_700_000_000),
            model: None,
            role: None,
            entry_count: 1,
            parent_session_id: Some(parent.to_owned()),
            parent_task_id: None,
            title: None,
            pinned: None,
            archived: None,
            writer: None,
            label: None,
        };
        for id in [
            "sub-busy",
            "sub-compact",
            "sub-idle",
            "sub-finished",
            "sub-stale",
            "sub-nolabel",
            "sub-multi",
        ] {
            let mut line = serde_json::to_vec(&sidecar(id, "web-parent")).unwrap();
            line.push(b'\n');
            std::fs::write(sessions_dir.join(format!("{id}.meta.jsonl")), line).unwrap();
        }
        // Combination 7: the parent-linked MAIN-registry session also has a
        // sidecar so the merge backfills its parent link.
        let mut parent_line =
            serde_json::to_vec(&sidecar("web-parent", "web-grandparent")).unwrap();
        parent_line.push(b'\n');
        std::fs::write(sessions_dir.join("web-parent.meta.jsonl"), parent_line).unwrap();
        // Combination 7b: a second parent-linked main-registry session, same
        // parent link, but with NO child-map entry at all — it keeps the
        // original "no handle must not reset busy" coverage.
        let mut nohandle_line =
            serde_json::to_vec(&sidecar("web-parent-nohandle", "web-grandparent")).unwrap();
        nohandle_line.push(b'\n');
        std::fs::write(
            sessions_dir.join("web-parent-nohandle.meta.jsonl"),
            nohandle_line,
        )
        .unwrap();

        // Label records (running_tasks stand-ins): sub-busy, sub-idle and
        // sub-stale carry one; sub-nolabel deliberately has none.
        let label_record = |id: &str, label: &str| serde_json::json!({ "id": 1, "label": label, "session_id": id });
        for (id, label) in [
            ("sub-busy", "任务-忙"),
            ("sub-idle", "任务-闲"),
            ("sub-stale", "任务-过期"),
        ] {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(sessions_dir.join("web-parent.background.jsonl"))
                .unwrap();
            writeln!(file, "{}", label_record(id, label)).unwrap();
        }

        // The parent in the main registry; its `Sessions` registry holds
        // the live subagent handles with test-controlled statuses. The
        // parent itself is Busy (combination 7 needs a real Busy status
        // from `session_meta` that must not be overwritten).
        let (parent_handle, parent_emitter, _parent_commands) =
            crate::runner::session_test_channel();
        parent_emitter.set_status(Status::Busy);
        let parent = live_session_with_handle(parent_handle);
        let parent_id = "web-parent".to_owned();
        let entry = |sid: &str, status: Status| {
            let (handle, emitter, _commands) = crate::runner::session_test_channel();
            emitter.set_status(status);
            (
                crate::delegate::SessionEntry {
                    handle,
                    model: "sub-model".into(),
                    role: None,
                    cwd: "/tmp".into(),
                    session_id: sid.to_owned(),
                    context_window: None,
                    store: SessionStore::Jsonl,
                },
                emitter,
            )
        };
        let mut task_id = 1u64;
        let mut insert = |sid: &str, status: Status| {
            let (entry, _emitter) = entry(sid, status);
            parent.sessions.insert(task_id, Arc::new(entry));
            task_id += 1;
        };
        insert("sub-busy", Status::Busy);
        insert("sub-compact", Status::Compacting);
        insert("sub-idle", Status::Idle);
        insert(
            "sub-finished",
            Status::Finished(crate::runner::SessionResult::Completed(None)),
        );
        insert("sub-nolabel", Status::Busy);
        // Combination 7: same-ID child handle for the main-registry parent,
        // conflicting Idle status — the main registry's Busy must win.
        insert("web-parent", Status::Idle);
        // Combination 8: two handles for the same id — Busy must win.
        insert("sub-multi", Status::Busy);
        insert("sub-multi", Status::Idle);

        let state = Arc::new(AppState {
            factory: crate::session_factory::SessionFactory::test_factory(root.clone()),
            registry: Arc::new(SessionRegistry::default()),
            token: "sekrit".into(),
            meta_store: SessionStore::Jsonl,
            summaries: Arc::new(Mutex::new(HashMap::new())),
            summary_pending: Arc::new(SummaryPending(Mutex::new(HashSet::new()))),
            shutdown: watch::channel(()).0,
        });
        state.registry.insert(parent_id, parent);
        // Combination 7b: a second parent-linked main-registry session with
        // no child-map entry; Busy from `session_meta` must stay.
        let (nohandle_handle, nohandle_emitter, _nohandle_commands) =
            crate::runner::session_test_channel();
        nohandle_emitter.set_status(Status::Busy);
        let nohandle = live_session_with_handle(nohandle_handle);
        state
            .registry
            .insert("web-parent-nohandle".to_owned(), nohandle);

        let listed = list_sessions(State(state)).await.0;
        let by_id: HashMap<&str, &SessionMeta> =
            listed.iter().map(|m| (m.id.as_str(), m)).collect();

        // 1) Child Busy.
        let m = by_id["sub-busy"];
        assert_eq!((m.active, m.status.as_str(), m.busy), (true, "Busy", true));
        assert_eq!(m.label.as_deref(), Some("任务-忙"));
        // 2) Child Compacting.
        let m = by_id["sub-compact"];
        assert_eq!(
            (m.active, m.status.as_str(), m.busy),
            (true, "Compacting", true)
        );
        // 3) Child Idle + label → live, not busy.
        let m = by_id["sub-idle"];
        assert_eq!((m.active, m.status.as_str(), m.busy), (true, "Idle", false));
        assert_eq!(m.label.as_deref(), Some("任务-闲"));
        // 4) Child Finished, cleanup not yet run → live, not busy.
        let m = by_id["sub-finished"];
        assert_eq!(
            (m.active, m.status.as_str(), m.busy),
            (true, "Finished", false)
        );
        // 5) Stale label, no child handle → stays inactive, busy=false.
        let m = by_id["sub-stale"];
        assert_eq!(
            (m.active, m.status.as_str(), m.busy),
            (false, "Idle", false)
        );
        assert_eq!(m.label.as_deref(), Some("任务-过期"));
        // 6) No label (stands in for a failed lookup) + child Busy → the
        //    handle-based backfill still applies.
        let m = by_id["sub-nolabel"];
        assert_eq!((m.active, m.status.as_str(), m.busy), (true, "Busy", true));
        assert_eq!(m.label, None);
        // 7) Parent-linked session IN the main registry, Busy from
        //    `session_meta`, plus a same-ID child handle at Idle → the
        //    main registry wins: the child handle must not overwrite it.
        let m = by_id["web-parent"];
        assert_eq!(m.parent_session_id.as_deref(), Some("web-grandparent"));
        assert_eq!((m.active, m.status.as_str(), m.busy), (true, "Busy", true));
        // 7b) Same, but NO child-map entry at all → busy likewise not
        //     overwritten (the original no-handle coverage).
        let m = by_id["web-parent-nohandle"];
        assert_eq!(m.parent_session_id.as_deref(), Some("web-grandparent"));
        assert_eq!((m.active, m.status.as_str(), m.busy), (true, "Busy", true));
        // 8) Two handles for one id: Busy beats the Idle sibling.
        let m = by_id["sub-multi"];
        assert_eq!((m.active, m.status.as_str(), m.busy), (true, "Busy", true));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The web addresses a subagent by session id exactly like the TUI:
    /// `live()` falls back from the main registry to every live session's
    /// `Sessions` registry, so history/SSE/prompt/cancel work on a
    /// subagent without it ever being in the registry.
    #[tokio::test]
    async fn live_falls_back_to_parent_subagent_registry() {
        let state = test_app_state("sekrit");
        let (parent_id, parent) = live_session("web-parent");
        let (handle, _emitter, _commands) = crate::runner::session_test_channel();
        let entry = Arc::new(crate::delegate::SessionEntry {
            handle,
            model: "sub-model".into(),
            role: None,
            cwd: "/tmp".into(),
            session_id: "sub-abc".into(),
            context_window: None,
            store: SessionStore::Jsonl,
        });
        parent.sessions.insert(7, entry);
        state.registry.insert(parent_id, parent.clone());

        // Registry ids still resolve as Live.
        assert!(matches!(
            live(&state, "web-parent").unwrap(),
            SessionRef::Live(_)
        ));
        // Subagent ids resolve through the parent's Sessions registry.
        match live(&state, "sub-abc").unwrap() {
            SessionRef::Subagent {
                entry,
                parent: found_parent,
                task_id,
            } => {
                assert_eq!(entry.session_id, "sub-abc");
                assert_eq!(entry.model, "sub-model");
                assert_eq!(task_id, 7, "the subagent's task id in the parent registry");
                assert!(
                    Arc::ptr_eq(&found_parent, &parent),
                    "parent must be the owning live session"
                );
            }
            _ => panic!("expected a Subagent ref, got a registry session"),
        }
        // Unknown ids still 404.
        assert!(live(&state, "ghost").is_err());
        // A subagent whose delegate task finished (Sessions entry removed)
        // is no longer resolvable live.
        parent.sessions.remove(7);
        assert!(live(&state, "sub-abc").is_err());
    }

    /// Delete is idempotent for unknown/historical ids: it removes the
    /// registry entry when present and hides the metadata rows (no-op on
    /// the Jsonl test store), returning 204 either way.
    #[tokio::test]
    async fn delete_session_hides_unknown_and_known() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        let state = test_app_state("sekrit");
        let app = router(state.clone());
        let unknown = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/sessions/some-historical-id")
                    .method("DELETE")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            unknown.status(),
            StatusCode::NO_CONTENT,
            "deleting an unknown session hides it idempotently"
        );
        // A live registry session also deletes cleanly.
        let (id, session) = live_session("web-del");
        state.registry.insert(id.clone(), session);
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/sessions/{id}"))
                    .method("DELETE")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(state.registry.get(&id).is_none(), "registry entry removed");
    }

    /// Regression (interrupt-no-longer-kills-delegate): `DELETE
    /// /api/sessions/{id}` on a live delegate subagent must TRULY terminate
    /// it. A plain `handle().cancel()` is a *release* — it preempts the
    /// in-flight operation and (for a FinishWhenIdle subagent with nothing
    /// queued) finalizes the runner as Cancelled, but it does not remove
    /// the `Sessions` registry entry / running_tasks row or abort a runner
    /// parked at an idle select — so the endpoint routes through the
    /// parent's `BackgroundTasks::cancel(task_id)` instead: the delegate
    /// wrapper is aborted, its captured cleanup removes the `Sessions`
    /// entry, and dropping the wrapper's runner handle aborts the subagent
    /// runner.
    #[tokio::test]
    async fn delete_subagent_session_aborts_delegate_runner_and_cleans_up() {
        use async_trait::async_trait;
        use axum::body::Body;
        use axum::http::Request;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::Notify;
        use tower::util::ServiceExt;

        use crate::agent::{Agent, AssistantMessage, ModelDeltaKind, ToolSpec, Usage};
        use crate::runner::{SessionResult, SessionRunner};

        // A subagent model that blocks inside complete() until the runner is
        // aborted: the subagent is Busy and can never finish on its own.
        struct BlockingModel {
            entered: Arc<Notify>,
            dropped: Arc<Notify>,
            side_effects: Arc<AtomicUsize>,
        }
        struct DropProbe(Arc<Notify>);
        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.notify_one();
            }
        }
        #[async_trait]
        impl Model for BlockingModel {
            async fn complete(
                &mut self,
                _: &[Message],
                _: &[ToolSpec],
                _: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
            ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
                let _probe = DropProbe(self.dropped.clone());
                self.entered.notify_one();
                std::future::pending::<()>().await;
                self.side_effects.fetch_add(1, Ordering::SeqCst);
                Ok((
                    AssistantMessage {
                        content: Some("too late".into()),
                        tool_calls: Vec::new(),
                        reasoning: None,
                    },
                    None,
                ))
            }
        }

        let state = test_app_state("sekrit");
        let (parent_id, parent, mut completions) =
            live_session_with_background_sender("web-parent-del");
        let temp = tempfile::tempdir().unwrap();

        // The delegate subagent runner (FinishWhenIdle), blocked in the model.
        let entered = Arc::new(Notify::new());
        let dropped = Arc::new(Notify::new());
        let side_effects = Arc::new(AtomicUsize::new(0));
        let agent = Agent::new(
            Box::new(BlockingModel {
                entered: entered.clone(),
                dropped: dropped.clone(),
                side_effects: side_effects.clone(),
            }),
            vec![],
        );
        let (runner, handle) = SessionRunner::new(
            agent,
            SessionStore::Jsonl,
            temp.path().into(),
            "sub-del".into(),
            IdlePolicy::FinishWhenIdle,
        );
        let runner_task = runner.start(Some("delegated task".into()));

        // Register the delegate in the parent's task panel the way
        // Delegate::execute does: spawn_with_id + Sessions entry in the
        // on_id hook; the work future mirrors the wrapper's runner_result
        // await, and `TestCleanup` mirrors DelegateCleanup — removed on the
        // normal-completion path AND on abort (Drop runs when the wrapper
        // future is dropped).
        struct TestCleanup {
            sessions: Sessions,
            slot: Arc<Mutex<Option<u64>>>,
        }
        impl Drop for TestCleanup {
            fn drop(&mut self) {
                if let Some(id) = self.slot.lock().unwrap().take() {
                    self.sessions.remove(id);
                }
            }
        }
        let sessions = parent.sessions.clone();
        let slot = Arc::new(Mutex::new(None::<u64>));
        let entry = Arc::new(crate::delegate::SessionEntry {
            handle: handle.clone(),
            model: "sub-model".into(),
            role: None,
            cwd: "/tmp".into(),
            session_id: "sub-del".into(),
            context_window: None,
            store: SessionStore::Jsonl,
        });
        let hook_sessions = sessions.clone();
        let hook_slot = slot.clone();
        let hook_entry = entry.clone();
        // Construct the guard BEFORE the spawn and capture it into the
        // wrapper, mirroring DelegateCleanup in delegate.rs: `entered`
        // only proves the subagent runner reached its model call, not that
        // the wrapper's work factory was invoked. If the DELETE abort lands
        // before the wrapper's first poll, the closure is dropped
        // un-invoked, and only a guard already captured in it can still
        // remove the Sessions entry.
        let cleanup = TestCleanup {
            sessions: sessions.clone(),
            slot: slot.clone(),
        };
        parent
            .background
            .spawn_with_id(
                "sub-del task".into(),
                None,
                None,
                None,
                crate::tools::new_exit_slot(),
                move |id| {
                    *hook_slot.lock().unwrap() = Some(id);
                    hook_sessions.insert(id, hook_entry);
                },
                move || {
                    let cleanup = cleanup;
                    async move {
                        // Inline of Delegate::runner_result (private).
                        let result = if let Err(error) = runner_task.join().await {
                            SessionResult::Failed(error.to_string())
                        } else {
                            match handle.status().borrow().clone() {
                                SessionStatus::Finished(result) => result,
                                _ => SessionResult::Closed,
                            }
                        };
                        drop(cleanup); // normal completion: remove the entry
                        match result {
                            SessionResult::Completed(answer) => answer.unwrap_or_default(),
                            SessionResult::Failed(error) => format!("subagent failed: {error}"),
                            SessionResult::Cancelled => "subagent cancelled".into(),
                            SessionResult::Closed => "subagent closed".into(),
                        }
                    }
                },
            )
            .unwrap();
        // The subagent must be inside its model call before the DELETE, so
        // the only way it can end is the abort.
        entered.notified().await;
        state.registry.insert(parent_id, parent.clone());
        assert_eq!(parent.background.running().len(), 1);
        assert_eq!(parent.sessions.list().len(), 1);

        let app = router(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/sessions/sub-del")
                    .method("DELETE")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        // The runner was truly aborted: its model future was dropped before
        // any side effect ran.
        dropped.notified().await;
        tokio::task::yield_now().await;
        assert_eq!(side_effects.load(Ordering::SeqCst), 0);
        // The task-panel registration and the Sessions entry are gone.
        assert!(parent.background.running().is_empty());
        assert!(parent.sessions.list().is_empty());
        // The parent saw the same "background task cancelled" completion a
        // task-panel cancel produces.
        assert!(matches!(
            completions.try_recv(),
            Ok(AgentEvent::BackgroundCompleted { output, .. })
                if output == "background task cancelled"
        ));
    }

    /// `POST /api/sessions/{id}/cancel` on a REAL runner — the endpoint was
    /// zero-tested (the /cancel route only ever exercised the fake
    /// `session_test_channel` handles). A FinishWhenIdle session Busy inside
    /// a blocking model call receives the cancel through the HTTP endpoint:
    /// 202 Accepted, the SSE stream the frontend consumes carries the
    /// "turn cancelled" Notice, the in-flight model future is preempted
    /// (drop probe), and with no prompt queued the runner finalizes
    /// Finished(Cancelled) — no "processing queued prompts" release branch,
    /// no prompt ever committed.
    #[tokio::test]
    async fn session_cancel_endpoint_releases_real_runner_and_finish_when_idle_finalizes_cancelled()
    {
        use async_trait::async_trait;
        use axum::http::Request;
        use futures_util::StreamExt;
        use tokio::sync::Notify;
        use tower::util::ServiceExt;

        use crate::agent::{Agent, AssistantMessage, ModelDeltaKind, ToolSpec, Usage};
        use crate::runner::{SessionResult, SessionRunner};

        // Blocks inside complete() until the cancel releases (drops) the
        // in-flight future; the drop probe proves the preemption landed.
        struct BlockingModel {
            entered: Arc<Notify>,
            dropped: Arc<Notify>,
        }
        struct DropProbe(Arc<Notify>);
        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.notify_one();
            }
        }
        #[async_trait]
        impl Model for BlockingModel {
            async fn complete(
                &mut self,
                _: &[Message],
                _: &[ToolSpec],
                _: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
            ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
                let _probe = DropProbe(self.dropped.clone());
                self.entered.notify_one();
                std::future::pending::<()>().await;
                Ok((
                    AssistantMessage {
                        content: Some("too late".into()),
                        tool_calls: Vec::new(),
                        reasoning: None,
                    },
                    None,
                ))
            }
        }

        let state = test_app_state("sekrit");
        let temp = tempfile::tempdir().unwrap();
        let entered = Arc::new(Notify::new());
        let dropped = Arc::new(Notify::new());
        let agent = Agent::new(
            Box::new(BlockingModel {
                entered: entered.clone(),
                dropped: dropped.clone(),
            }),
            vec![],
        );
        let (runner, handle) = SessionRunner::new(
            agent,
            SessionStore::Jsonl,
            temp.path().into(),
            "web-cancel".into(),
            IdlePolicy::FinishWhenIdle,
        );
        // Wrap the REAL runner in a LiveSession exactly like `create_session`
        // does (real task, not the fake `session_test_channel` spawn).
        let runner_task = runner.start(Some("initial".into()));
        let workspace = crate::workspace::Workspace::new(std::env::temp_dir()).unwrap();
        let (_tools, background) = crate::tools::builtins(workspace, None, false, None);
        let session = Arc::new(LiveSession {
            handle: handle.clone(),
            task: runner_task,
            store: SessionStore::Jsonl,
            background,
            sessions: Sessions::default(),
            model_name: Mutex::new("test-model".into()),
            role_name: None,
            created_at: chrono::Utc::now(),
        });
        state.registry.insert("web-cancel".into(), session);

        // The runner is Busy inside its model call before either HTTP call.
        entered.notified().await;
        let app = router(state.clone());

        // SSE subscriber first: the stream must carry the "turn cancelled"
        // Notice the frontend renders when a user hits the cancel button.
        let sse = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/sessions/web-cancel/events")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(sse.status(), StatusCode::OK);
        let mut sse_body = sse.into_body().into_data_stream();

        // POST /cancel -> 202 Accepted.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/sessions/web-cancel/cancel")
                    .method("POST")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        // The release preempts the in-flight model future (drop probe).
        dropped.notified().await;

        // The SSE stream carries the "turn cancelled" Notice.
        let mut buffered = String::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, sse_body.next()).await {
                Err(_) => panic!("SSE stream timeout before 'turn cancelled'"),
                Ok(None) => panic!("SSE stream ended before 'turn cancelled'"),
                Ok(Some(Err(error))) => panic!("SSE stream error: {error}"),
                Ok(Some(Ok(bytes))) => {
                    buffered.push_str(&String::from_utf8_lossy(&bytes));
                    if buffered.contains("turn cancelled") {
                        break;
                    }
                }
            }
        }
        // FWI + no queued prompt: the session finalizes Cancelled.
        let mut status = handle.status();
        let result = loop {
            let value = status.borrow().clone();
            if matches!(value, SessionStatus::Finished(_)) {
                break value;
            }
            status.changed().await.unwrap();
        };
        assert_eq!(result, SessionStatus::Finished(SessionResult::Cancelled));

        // The cancel never took the ReleasedWithPrompts path: no "processing
        // queued prompts" Notice and no committed prompt beyond the initial
        // one.
        assert!(
            !handle.snapshot().iter().any(|event| matches!(
                event,
                AgentEvent::Notice(text) if text == "processing queued prompts"
            )),
            "no queued prompt: the release must not take the prompts branch"
        );
        let loaded = SessionStore::Jsonl
            .load(temp.path(), "web-cancel")
            .await
            .unwrap();
        assert!(
            !loaded.entries.iter().any(|entry| matches!(
                entry,
                SessionEntry::Message {
                    message: Message::User { content, .. }
                } if content != "initial"
            )),
            "no prompt may be committed after the cancel"
        );
        drop(sse_body);
        state.registry.remove("web-cancel");
    }

    /// The frontend contract (contract item 5): `SessionEntry` is internally
    /// tagged with `type` and `Message` stays externally tagged, so a history
    /// page serializes as `{type:"message", message:{User:{content,images?}}}`.
    /// The `{entries, next_before_seq}` wrapper is compatible with the
    /// frontend's defensive parsing (bare array or `{entries:[...]}`).
    #[test]
    fn history_response_wire_shape() {
        use crate::agent::{Message, SessionEntry};
        let entries = vec![
            SessionEntry::Message {
                message: Message::User {
                    content: "hello".into(),
                    images: vec![],
                },
            },
            SessionEntry::Compaction {
                summary: "rolled up".into(),
                retained: vec![],
                current_prompt_at: None,
                no_current_prompt: false,
            },
        ];
        assert_eq!(
            serde_json::to_value(HistoryResponse {
                entries,
                next_before_seq: Some(42),
            })
            .unwrap(),
            serde_json::json!({
                "entries": [
                    {"type": "message", "message": {"User": {"content": "hello"}}},
                    {"type": "compaction", "summary": "rolled up", "retained": []},
                ],
                "next_before_seq": 42,
            })
        );
        // No compaction: the whole session is one head segment, nothing
        // older to page — cursor must serialize as JSON null.
        assert_eq!(
            serde_json::to_value(HistoryResponse {
                entries: vec![],
                next_before_seq: None,
            })
            .unwrap(),
            serde_json::json!({"entries": [], "next_before_seq": null})
        );
    }

    /// M3: snapshots are capped at the newest `SNAPSHOT_MAX` events so a
    /// long session's unbounded in-memory log cannot blow up the per-client
    /// snapshot/resync frame.
    #[test]
    fn snapshot_tail_keeps_newest_events() {
        let events: Vec<AgentEvent> = (0..SNAPSHOT_MAX + 50)
            .map(|i| AgentEvent::Notice(format!("n{i}")))
            .collect();
        let tail = tail_snapshot(events);
        assert_eq!(tail.len(), SNAPSHOT_MAX);
        assert_eq!(tail.first(), Some(&AgentEvent::Notice("n50".into())));
        assert_eq!(tail.last(), Some(&AgentEvent::Notice("n1049".into())));
        // Under the cap: unchanged.
        let small = vec![AgentEvent::Notice("x".into())];
        assert_eq!(tail_snapshot(small).len(), 1);
    }

    /// M5: `require_auth` rejects missing/wrong credentials with 401 and
    /// lets the startup token through (Bearer header or `?token=`) on the
    /// real router, so every `/api/*` route is covered by the middleware.
    #[tokio::test]
    async fn require_auth_rejects_missing_and_wrong_token() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        let app = router(test_app_state("sekrit"));
        let status =
            |request: Request<Body>| async { app.clone().oneshot(request).await.unwrap().status() };
        assert_eq!(
            status(
                Request::builder()
                    .uri("/api/sessions")
                    .body(Body::empty())
                    .unwrap()
            )
            .await,
            StatusCode::UNAUTHORIZED,
            "no token must be rejected"
        );
        assert_eq!(
            status(
                Request::builder()
                    .uri("/api/sessions")
                    .header(header::AUTHORIZATION, "Bearer nope")
                    .body(Body::empty())
                    .unwrap()
            )
            .await,
            StatusCode::UNAUTHORIZED,
            "wrong bearer token must be rejected"
        );
        assert_eq!(
            status(
                Request::builder()
                    .uri("/api/sessions")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap()
            )
            .await,
            StatusCode::OK,
            "right bearer token must pass"
        );
        assert_eq!(
            status(
                Request::builder()
                    .uri("/api/sessions?token=sekrit")
                    .body(Body::empty())
                    .unwrap()
            )
            .await,
            StatusCode::OK,
            "right query token must pass"
        );
    }

    /// M5: `session_history` for an unknown session id returns 404 — the
    /// registry-miss path short-circuits before any store I/O, so this is
    /// testable without a real store.
    #[tokio::test]
    async fn session_history_404_for_unknown_session() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        let app = router(test_app_state("sekrit"));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/sessions/does-not-exist/history")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// M5: a historical session — entries persisted, no live registry
    /// entry — must be readable via `session_history` without resuming it
    /// first (the 404-registry-miss now falls back to a direct store
    /// connect; 404 is reserved for transcripts that truly do not exist).
    /// Exercised on the JSONL backend: write a session file, then hit the
    /// handler with a state whose registry is empty.
    #[tokio::test]
    async fn session_history_reads_historical_session_without_registry() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        use crate::agent::{Message, SessionEntry};
        use crate::session::Session;

        // Own temp root: the shared test_app_state root must not receive
        // stray session files (parallel tests share it).
        let root =
            std::env::temp_dir().join(format!("e-agent-server-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let id = "historical-subagent";
        Session::append(
            &root,
            id,
            &[SessionEntry::Message {
                message: Message::User {
                    content: "hello from the past".into(),
                    images: vec![],
                },
            }],
        )
        .unwrap();

        let state = Arc::new(AppState {
            factory: crate::session_factory::SessionFactory::test_factory(root.clone()),
            registry: Arc::new(SessionRegistry::default()),
            token: "sekrit".to_owned(),
            meta_store: SessionStore::Jsonl,
            summaries: Arc::new(Mutex::new(HashMap::new())),
            summary_pending: Arc::new(SummaryPending(Mutex::new(HashSet::new()))),
            shutdown: watch::channel(()).0,
        });
        let app = router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/sessions/{id}/history"))
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            value["entries"],
            serde_json::json!([
                {"type": "message", "message": {"User": {"content": "hello from the past"}}}
            ])
        );
        assert_eq!(value["next_before_seq"], serde_json::Value::Null);
        let _ = std::fs::remove_dir_all(&root);
    }

    // --- Web fork panel: `fork-candidates` + `fork` ---------------------

    fn user_message(content: &str) -> SessionEntry {
        SessionEntry::Message {
            message: Message::User {
                content: content.to_owned(),
                images: vec![],
            },
        }
    }

    fn assistant_message(content: &str) -> SessionEntry {
        use crate::agent::AssistantMessage;
        SessionEntry::Message {
            message: Message::Assistant(AssistantMessage {
                content: Some(content.to_owned()),
                tool_calls: vec![],
                reasoning: None,
            }),
        }
    }

    /// An assistant message with pending tool calls — NOT a turn boundary.
    fn assistant_with_tool_calls() -> SessionEntry {
        use crate::agent::{AssistantMessage, ToolCall};
        SessionEntry::Message {
            message: Message::Assistant(AssistantMessage {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "bash".into(),
                    arguments: "ls".into(),
                }],
                reasoning: None,
            }),
        }
    }

    fn tool_message(name: &str, content: &str) -> SessionEntry {
        SessionEntry::Message {
            message: Message::Tool {
                call_id: "call_1".into(),
                name: name.to_owned(),
                content: content.to_owned(),
                is_error: false,
                synthetic: false,
                images: vec![],
            },
        }
    }

    /// A fresh AppState whose factory root is an isolated temp dir (parallel
    /// tests share the default test root, so fork tests write their own
    /// session files to a private one).
    fn fork_test_state(tag: &str) -> (Arc<AppState>, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("e-agent-server-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let state = Arc::new(AppState {
            factory: crate::session_factory::SessionFactory::test_factory(root.clone()),
            registry: Arc::new(SessionRegistry::default()),
            token: "sekrit".to_owned(),
            meta_store: SessionStore::Jsonl,
            summaries: Arc::new(Mutex::new(HashMap::new())),
            summary_pending: Arc::new(SummaryPending(Mutex::new(HashSet::new()))),
            shutdown: watch::channel(()).0,
        });
        (state, root)
    }

    /// `entry_preview` extracts display text per entry kind, truncated to
    /// ≤ 80 chars: user/assistant content verbatim, mid-turn tool steps as
    /// "name: content", compactions with a 📦 prefix, and everything else
    /// a fixed system marker.
    #[test]
    fn entry_preview_extracts_display_text() {
        let long = "字".repeat(100);
        assert_eq!(entry_preview(&user_message("你好")), "你好");
        assert_eq!(entry_preview(&assistant_message("回答")), "回答");
        assert_eq!(
            entry_preview(&assistant_with_tool_calls()),
            "（工具调用步骤）"
        );
        assert_eq!(entry_preview(&tool_message("bash", "ls")), "bash: ls");
        assert_eq!(
            entry_preview(&SessionEntry::Compaction {
                summary: "前情".into(),
                retained: vec![],
                current_prompt_at: None,
                no_current_prompt: false,
            }),
            "📦 压缩：前情"
        );
        assert_eq!(
            entry_preview(&SessionEntry::Notice {
                text: "note".into()
            }),
            "（系统条目）"
        );
        // Truncation: ≤ 80 chars with an ellipsis, tail kept for identity.
        let preview = entry_preview(&user_message(&long));
        assert!(preview.chars().count() <= 80, "{preview}");
        assert!(preview.contains('…'), "{preview}");
    }

    /// Web fork panel: `GET .../fork-candidates` lists every turn boundary
    /// of a session — assistant messages with no pending tool calls only —
    /// with the 1-based `at`, the backend seq, and a truncated preview.
    /// User, tool and mid-turn assistant entries are skipped.
    #[tokio::test]
    async fn fork_candidates_lists_turn_boundaries() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        use crate::session::Session;

        let (state, root) = fork_test_state("fork-candidates");
        let id = "fork-candidates-src";
        Session::append(
            &root,
            id,
            &[
                user_message("第一问"),
                assistant_message("第一答"),
                user_message("第二问"),
                assistant_with_tool_calls(),
                tool_message("bash", "ok"),
                assistant_message("第二答"),
            ],
        )
        .unwrap();
        let app = router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/sessions/{id}/fork-candidates"))
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // Only the two completed turns qualify; at is 1-based, seq is the
        // 0-based JSONL line index.
        assert_eq!(
            value,
            serde_json::json!([
                {"at": 2, "seq": 1, "preview": "第一答"},
                {"at": 6, "seq": 5, "preview": "第二答"},
            ])
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Web fork panel: unknown session id → 404, same as `session_history`.
    #[tokio::test]
    async fn fork_candidates_404_unknown() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        let app = router(test_app_state("sekrit"));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/sessions/does-not-exist/fork-candidates")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// `POST .../fork` at a turn boundary copies the source prefix (1-based,
    /// inclusive) plus a `forked_from` marker into a fresh `fork-…` session,
    /// registers it as live (immediately readable via history), and returns
    /// 201 + the new id.
    #[tokio::test]
    async fn fork_at_boundary_creates_session() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        use crate::session::Session;

        let (state, root) = fork_test_state("fork-create");
        let id = "fork-src-session";
        let source = [
            user_message("hello"),
            assistant_message("hi there"),
            user_message("第二个问题"),
            assistant_message("第二个回答"),
        ];
        Session::append(&root, id, &source).unwrap();
        let app = router(state);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/sessions/{id}/fork"))
                    .method("POST")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"at": 4}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let new_id = value["id"].as_str().expect("fork id").to_owned();
        assert!(new_id.starts_with("fork-"), "{new_id}");

        // The fork- session is registered and its history is the source
        // prefix (first 4 entries) plus the forked_from marker at the cut.
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/sessions/{new_id}/history"))
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let entries: Vec<SessionEntry> = serde_json::from_value(value["entries"].clone()).unwrap();
        assert_eq!(entries.len(), source.len() + 1);
        assert_eq!(entries[..source.len()], source);
        assert_eq!(
            entries[source.len()],
            SessionEntry::ForkedFrom {
                source: id.to_owned(),
                at: 4,
                event_time: None,
                seq: Some(3),
            }
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `POST .../fork` rejects an `at` that points at an assistant message
    /// with pending tool calls (mid-turn) with 409 + a Chinese error.
    #[tokio::test]
    async fn fork_rejects_non_boundary_at() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        use crate::session::Session;

        let (state, root) = fork_test_state("fork-nonboundary");
        let id = "fork-nonboundary-src";
        Session::append(
            &root,
            id,
            &[
                user_message("q1"),
                assistant_message("a1"),
                user_message("q2"),
                assistant_with_tool_calls(), // index 4 (1-based) — mid-turn
                tool_message("bash", "ok"),
                assistant_message("a2"),
            ],
        )
        .unwrap();
        let app = router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/sessions/{id}/fork"))
                    .method("POST")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"at": 4}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let message = String::from_utf8(body.to_vec()).unwrap();
        assert!(message.contains("无法 fork"), "{message}");
        assert!(message.contains("not a turn boundary"), "{message}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `POST .../fork` rejects an `at` beyond the last entry with 409.
    #[tokio::test]
    async fn fork_rejects_out_of_range() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        use crate::session::Session;

        let (state, root) = fork_test_state("fork-range");
        let id = "fork-range-src";
        Session::append(&root, id, &[user_message("q1"), assistant_message("a1")]).unwrap();
        let app = router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/sessions/{id}/fork"))
                    .method("POST")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"at": 99}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let message = String::from_utf8(body.to_vec()).unwrap();
        assert!(message.contains("无法 fork"), "{message}");
        assert!(message.contains("out of range"), "{message}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `POST .../fork` on an unknown session id → 404 before any build work.
    #[tokio::test]
    async fn fork_unknown_session_404() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        let app = router(test_app_state("sekrit"));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/sessions/does-not-exist/fork")
                    .method("POST")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"at": 1}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// `POST .../fork` with a missing `at` or `at: 0` → 400.
    #[tokio::test]
    async fn fork_missing_at_400() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        let app = router(test_app_state("sekrit"));
        let response = |body: String| {
            let app = app.clone();
            async move {
                app.oneshot(
                    Request::builder()
                        .uri("/api/sessions/some-session/fork")
                        .method("POST")
                        .header(header::AUTHORIZATION, "Bearer sekrit")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap()
            }
        };
        let missing = response(r#"{}"#.to_owned()).await;
        assert_eq!(missing.status(), StatusCode::BAD_REQUEST, "missing at");
        let zero = response(r#"{"at": 0}"#.to_owned()).await;
        assert_eq!(zero.status(), StatusCode::BAD_REQUEST, "at = 0");
    }

    /// M5: `session_history` with a `before_seq` cursor must forward
    /// `limit` to `load_older` and surface the returned cursor. Exercised
    /// with the JSONL store on a session with no persisted entries, whose
    /// `load_older` answers empty + `None` — the endpoint must still
    /// accept `before_seq` + `limit` together and serialize the page (the
    /// paging itself is covered by `session_sqlite`'s tests and the
    /// JSONL positional-paging tests in `session_store`).
    #[tokio::test]
    async fn session_history_pages_older_with_limit() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        let state = test_app_state("sekrit");
        let (id, session) = live_session("web-paged");
        state.registry.insert(id.clone(), session);
        let app = router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/sessions/{id}/history?before_seq=42&limit=200"
                    ))
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["entries"], serde_json::json!([]));
        assert_eq!(value["next_before_seq"], serde_json::Value::Null);
    }

    /// M5: `HistoryParams` must deserialize from the query strings the
    /// frontend sends — both fields optional, `before_seq` an i64 paging
    /// cursor, `limit` a cap.
    #[tokio::test]
    async fn history_params_parse_from_query_string() {
        use axum::extract::FromRequestParts;
        use axum::http::Request;

        let parse = |query: String| async move {
            let request = Request::builder()
                .uri(format!("/api/sessions/x/history?{query}"))
                .body(())
                .unwrap();
            let (mut parts, _) = request.into_parts();
            let Ok(Query(params)) =
                Query::<HistoryParams>::from_request_parts(&mut parts, &()).await
            else {
                panic!("query {query} must parse");
            };
            params
        };
        let none = parse(String::new()).await;
        assert_eq!(none.before_seq, None);
        assert_eq!(none.limit, None);
        let both = parse("before_seq=42&limit=10".to_owned()).await;
        assert_eq!(both.before_seq, Some(42));
        assert_eq!(both.limit, Some(10));
    }

    /// M5: a client that falls behind the broadcast buffer must receive a
    /// fresh `event: resync` carrying the event log (so the frontend
    /// force-replaces its transcript), followed by a fresh status — then the
    /// stream keeps serving on the new receiver.
    #[tokio::test]
    async fn forward_events_resyncs_when_client_lags() {
        let (handle, emitter, _commands) = crate::runner::session_test_channel();
        let id = "web-lag".to_owned();
        let session = live_session_with_handle(handle.clone());
        let state = test_app_state("sekrit");
        state.registry.insert(id.clone(), session.clone());

        // Subscribe, then overflow the broadcast buffer (capacity 256) while
        // the receiver idles so its next recv reports Lagged.
        let (snapshot, live, status) = handle.attach();
        for i in 0..300 {
            emitter.emit(AgentEvent::Notice(format!("n{i}")));
        }
        drop(emitter);

        let (tx, mut rx) = mpsc::channel::<Result<Event, Error>>(16);
        let task = tokio::spawn(forward_events(
            state.clone(),
            id.clone(),
            tail_snapshot(snapshot),
            live,
            status,
            state.shutdown.subscribe(),
            tx,
        ));

        // Frame order: snapshot + status first, then the lag resync + a
        // fresh status.
        let first = event_to_text(rx.recv().await.unwrap().unwrap()).await;
        assert!(first.contains("event: snapshot"), "{first}");
        let second = event_to_text(rx.recv().await.unwrap().unwrap()).await;
        assert!(second.contains("event: status"), "{second}");
        let third = event_to_text(rx.recv().await.unwrap().unwrap()).await;
        assert!(
            third.contains("event: resync"),
            "lag must emit resync:\n{third}"
        );
        // The resync replays the log tail: all 300 events (under SNAPSHOT_MAX).
        let data = third
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("resync data line");
        let events: Vec<serde_json::Value> = serde_json::from_str(data).unwrap();
        assert_eq!(events.len(), 300);
        let fourth = event_to_text(rx.recv().await.unwrap().unwrap()).await;
        assert!(fourth.contains("event: status"), "{fourth}");

        // Drop every sender: the loop sees the closed channels and exits.
        drop(rx);
        state.registry.remove(&id);
        drop(session);
        drop(handle);
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("forward_events must exit once the session is gone")
            .expect("forward_events task must not panic");
    }

    /// M5: the bounded queue disconnects a client that cannot keep up —
    /// with the queue already full, `forward_events` ends immediately
    /// instead of buffering without bound (the frontend then reconnects).
    #[tokio::test]
    async fn forward_events_disconnects_when_queue_is_full() {
        let (handle, _emitter, _commands) = crate::runner::session_test_channel();
        let (id, session) = live_session("web-full");
        let state = test_app_state("sekrit");
        state.registry.insert(id.clone(), session.clone());
        let (snapshot, live, status) = handle.attach();

        // Capacity 1, already occupied: the very first try_send fails and
        // the stream ends.
        let (tx, _rx) = mpsc::channel::<Result<Event, Error>>(1);
        let _ = tx.try_send(Ok(Event::default().comment("prefill")));
        let shutdown = state.shutdown.subscribe();
        let task = tokio::spawn(forward_events(
            state,
            id,
            tail_snapshot(snapshot),
            live,
            status,
            shutdown,
            tx,
        ));
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("forward_events must end when the queue is full")
            .expect("forward_events task must not panic");
    }

    /// Ctrl-C shutdown（方案 B）：shutdown watch 一旦触发，`forward_events`
    /// 必须自己结束流——SSE 连接随之关闭，graceful drain 毫秒级完成，
    /// SHUTDOWN_DRAIN_TIMEOUT 硬截止线只是兜底，而不是每次 Ctrl-C 都要等满
    /// 2s（浏览器标签页挂着的 /events 流也关得掉）。
    #[tokio::test]
    async fn forward_events_ends_stream_on_shutdown() {
        let (handle, _emitter, _commands) = crate::runner::session_test_channel();
        let (id, session) = live_session("web-shutdown");
        let state = test_app_state("sekrit");
        state.registry.insert(id.clone(), session.clone());
        let (snapshot, live, status) = handle.attach();

        let (tx, mut rx) = mpsc::channel::<Result<Event, Error>>(16);
        let shutdown_rx = state.shutdown.subscribe();
        let task = tokio::spawn(forward_events(
            state.clone(),
            id,
            tail_snapshot(snapshot),
            live,
            status,
            shutdown_rx,
            tx,
        ));

        // 流活着：初始 snapshot + status 两帧先到（session_events 的既定
        // 帧序），都消费掉再触发 shutdown。
        let first = event_to_text(rx.recv().await.unwrap().unwrap()).await;
        assert!(first.contains("event: snapshot"), "{first}");
        let second = event_to_text(rx.recv().await.unwrap().unwrap()).await;
        assert!(second.contains("event: status"), "{second}");

        // 触发 shutdown（Ctrl-C 时 `run` 里的 shutdown future 会 send 同一
        // 个 sender）：流必须立即自结束，而不是等会话被删。
        let _ = state.shutdown.send(());
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("forward_events must end its stream once shutdown is signaled")
            .expect("forward_events task must not panic");
        // 响应流也随之结束：tx 已随 forward_events 返回而 drop，缓冲清空后
        // recv 返回 None。
        assert!(
            rx.recv().await.is_none(),
            "SSE stream must be closed after shutdown"
        );
    }

    /// M5 contract (frontend): each live SSE frame must pair the CamelCase
    /// event name (`applyLiveEvent` switch) with the flat payload keys
    /// (`pickText` / per-event handlers) on the actual wire — a name/payload
    /// mismatch would silently break rendering.
    #[tokio::test]
    async fn live_event_wire_frames_match_frontend_contract() {
        use serde_json::json;
        // GoalUpdated：完整快照（set）与 null 墓碑（clear）都必须是
        // `event: GoalUpdated` + 扁平 `{"goal": <snapshot|null>}` ——
        // frontend 的 applyLiveEvent GoalUpdated 分支按此契约刷新 GoalBar。
        let goal = crate::agent::GoalSnapshot {
            id: "g1".into(),
            revision: 2,
            objective: "ship it".into(),
            success_criteria: vec!["tests pass".into()],
            status: crate::agent::GoalStatus::Paused,
            progress: "half".into(),
            evidence: vec![],
            blocked_reason: Some("waiting".into()),
        };
        let cases: &[(AgentEvent, &str, serde_json::Value)] = &[
            (
                AgentEvent::UserPrompt("hi".into()),
                "UserPrompt",
                json!({"text": "hi"}),
            ),
            (
                AgentEvent::AssistantText("done".into()),
                "AssistantText",
                json!({"text": "done"}),
            ),
            (
                AgentEvent::AssistantDelta("正在".into()),
                "AssistantDelta",
                json!({"delta": "正在"}),
            ),
            (
                AgentEvent::ReasoningDelta("推理".into()),
                "ReasoningDelta",
                json!({"delta": "推理"}),
            ),
            (
                AgentEvent::Notice("note".into()),
                "Notice",
                json!({"text": "note"}),
            ),
            (
                AgentEvent::Error("boom".into()),
                "Error",
                json!({"error": "boom"}),
            ),
            (
                AgentEvent::ToolCall {
                    name: "bash".into(),
                    arguments: "ls".into(),
                },
                "ToolCall",
                json!({"name": "bash", "arguments": "ls"}),
            ),
            (
                AgentEvent::ToolResult {
                    is_error: true,
                    content: "no".into(),
                },
                "ToolResult",
                json!({"is_error": true, "content": "no"}),
            ),
            (
                AgentEvent::GoalUpdated {
                    goal: Some(goal.clone()),
                },
                "GoalUpdated",
                json!({ "goal": goal }),
            ),
            (
                AgentEvent::GoalUpdated { goal: None },
                "GoalUpdated",
                json!({ "goal": null }),
            ),
        ];
        for (event, name, payload) in cases {
            let text = event_to_text(live_event(event).unwrap()).await;
            assert!(
                text.contains(&format!("event: {name}")),
                "frame for {name} must carry the CamelCase event name:\n{text}"
            );
            let payload_text = payload.to_string();
            assert!(
                text.contains(&payload_text),
                "frame for {name} must carry the flat payload {payload_text}:\n{text}"
            );
        }
        // Status frames: the frontend reads `JSON.parse(data).status`.
        let text = event_to_text(status_event(&SessionStatus::Busy).unwrap()).await;
        assert!(text.contains("event: status"), "{text}");
        assert!(text.contains(r#"{"status":"Busy"}"#), "{text}");
    }

    /// The `task_meta` field mapping: bash output becomes lossy UTF-8
    /// truncated at `TASK_OUTPUT_LIMIT` chars, and delegate display metadata
    /// is flattened into `background`, `workspace`, `subagent_session_id`
    /// and `resume` (all empty for non-delegate tasks). `owner_session` comes
    /// straight from `BackgroundTaskInfo`; `None` falls back to the listing
    /// `session_id` so the frontend always has an initiator id.
    #[test]
    fn task_meta_maps_fields_and_truncates_output() {
        let long = task_meta(
            "web-x",
            BackgroundTaskInfo {
                id: 7,
                label: "big".into(),
                full_command: Some("yes".into()),
                role: Some("coder".into()),
                kind: "bash".into(),
                output: vec![b'a'; 5000],
                display_meta: None,
                owner_session: None,
                started_at_ms: None,
            },
        );
        assert_eq!(long.session_id, "web-x");
        assert_eq!(long.id, 7);
        assert_eq!(long.label, "big");
        assert_eq!(long.full_command.as_deref(), Some("yes"));
        assert_eq!(long.role.as_deref(), Some("coder"));
        assert_eq!(long.kind, "bash");
        assert_eq!(
            long.output.chars().count(),
            crate::session_store::TASK_OUTPUT_LIMIT
        );
        assert!(!long.background, "non-delegate tasks have no display meta");
        assert_eq!(long.workspace, None);
        assert_eq!(long.subagent_session_id, None);
        assert_eq!(long.resume, None);
        assert_eq!(
            long.owner_session.as_deref(),
            Some("web-x"),
            "unknown owner falls back to the listing session id"
        );
        // Invalid UTF-8 becomes the lossy replacement character; short
        // output passes through untruncated.
        let lossy = task_meta(
            "web-x",
            BackgroundTaskInfo {
                id: 8,
                label: "bytes".into(),
                full_command: None,
                role: None,
                kind: "bash".into(),
                output: vec![0xff, 0xfe],
                display_meta: None,
                owner_session: None,
                started_at_ms: None,
            },
        );
        assert_eq!(lossy.output, "\u{FFFD}\u{FFFD}");
        // Delegate display metadata is flattened into background + workspace
        // and the subagent session id passes through for direct jumps.
        let delegate = task_meta(
            "web-y",
            BackgroundTaskInfo {
                id: 9,
                label: "d".into(),
                full_command: None,
                role: None,
                kind: "delegate".into(),
                output: Vec::new(),
                display_meta: Some(TaskDisplayMeta {
                    background: true,
                    workspace: Some("/tmp/w".into()),
                    subagent_session_id: Some("sub-abc".into()),
                    resume: Some("sub-resume-1".into()),
                }),
                owner_session: None,
                started_at_ms: None,
            },
        );
        assert_eq!(delegate.kind, "delegate");
        assert!(delegate.background);
        assert_eq!(delegate.workspace.as_deref(), Some("/tmp/w"));
        assert_eq!(delegate.subagent_session_id.as_deref(), Some("sub-abc"));
        assert_eq!(delegate.resume.as_deref(), Some("sub-resume-1"));
        // A subagent's bash task carries its own session id as the owner,
        // which wins over the registry owner (`session_id`).
        let subagent_bash = task_meta(
            "web-parent",
            BackgroundTaskInfo {
                id: 10,
                label: "build".into(),
                full_command: Some("cargo build".into()),
                role: None,
                kind: "bash".into(),
                output: Vec::new(),
                display_meta: None,
                owner_session: Some("sub-xyz".into()),
                started_at_ms: None,
            },
        );
        assert_eq!(
            subagent_bash.owner_session.as_deref(),
            Some("sub-xyz"),
            "subagent bash task exposes its own session as the initiator"
        );
    }

    /// `GET /api/tasks` on an empty registry (and on sessions with no
    /// running tasks) returns an empty list.
    #[tokio::test]
    async fn list_tasks_empty_registry_returns_empty_list() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        let state = test_app_state("sekrit");
        // One live session, no running tasks.
        let (id, session) = live_session("web-idle");
        state.registry.insert(id, session);
        let app = router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/tasks")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            serde_json::json!([])
        );
    }

    /// `GET /api/tasks/finished` — with the JSONL meta store (test
    /// default) there is no `session_entries` table, so the endpoint
    /// returns an empty list instead of erroring. On Greptime/SQLite it
    /// reads the durable `background_completion` rows newest-first (the
    /// store-level query is covered by
    /// `finished_tasks_reads_background_completion_entries_newest_first`).
    #[tokio::test]
    async fn finished_tasks_endpoint_returns_list_with_jsonl_meta_store() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        let state = test_app_state("sekrit");
        let (id, session) = live_session("web-idle");
        state.registry.insert(id, session);
        let app = router(state);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/tasks/finished")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            serde_json::json!([])
        );

        // The live /api/tasks endpoint is unchanged (still registry-backed).
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/tasks")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// `DELETE /api/sessions/{id}/tasks/{task_id}` returns 404 for an
    /// unknown session and for a known session with no such task.
    #[tokio::test]
    async fn cancel_task_404_for_unknown_session_and_unknown_task() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        let state = test_app_state("sekrit");
        let (id, session) = live_session("web-cancel");
        state.registry.insert(id.clone(), session);
        let app = router(state);
        let status = |uri: String| async {
            let app = app.clone();
            app.oneshot(
                Request::builder()
                    .uri(uri)
                    .method("DELETE")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
        };
        assert_eq!(
            status("/api/sessions/ghost/tasks/1".to_owned()).await,
            StatusCode::NOT_FOUND,
            "unknown session must 404"
        );
        assert_eq!(
            status(format!("/api/sessions/{id}/tasks/42")).await,
            StatusCode::NOT_FOUND,
            "unknown task id must 404"
        );
    }

    /// `GET /api/sessions/{id}/tasks/{task_id}/output` serves the full
    /// captured output of a running bash task as text/plain with
    /// `Cache-Control: no-store` (the global API middleware upgrades the
    /// handler's `no-cache`; proving it is the full spool, not the
    /// 16 KiB tail `/api/tasks` serves), and 404s for unknown sessions,
    /// subagent sessions (no registry of their own), unknown task ids, and
    /// delegate tasks (no output spool).
    #[tokio::test]
    async fn task_output_serves_full_output_and_404s() {
        use axum::body::Body;
        use axum::http::Request;
        use std::time::Duration;
        use tower::util::ServiceExt;

        let state = test_app_state("sekrit");
        let (id, session, _rx) = live_session_with_background_sender("web-out");
        state.registry.insert(id.clone(), session.clone());
        let app = router(state);

        // Unknown session → 404.
        let ghost = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/sessions/ghost/tasks/1/output")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ghost.status(), StatusCode::NOT_FOUND);

        // A live bash task printing 100 KB (well past the 16 KiB tail
        // slot) and staying alive while we poll.
        let workspace = crate::workspace::Workspace::new(std::env::temp_dir()).unwrap();
        session
            .background
            .start(
                workspace,
                "head -c 100000 /dev/zero | tr '\\0' a; printf '\\nend\\n'; sleep 30".to_string(),
                false,
            )
            .expect("bash background task starts");
        let uri = format!("/api/sessions/{id}/tasks/1/output");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let (headers, body) = loop {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(&uri)
                        .header(header::AUTHORIZATION, "Bearer sekrit")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            if response.status() == StatusCode::OK {
                let headers = response.headers().clone();
                let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
                    .await
                    .unwrap();
                if String::from_utf8_lossy(&bytes).ends_with("\nend\n") {
                    break (headers, bytes);
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "full task output never became available"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert_eq!(
            headers
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store"),
            "polling endpoint must not be cached (global API middleware upgrades the handler's no-cache to no-store)"
        );
        assert_eq!(
            headers
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/plain; charset=utf-8")
        );
        let text = String::from_utf8_lossy(&body);
        assert_eq!(text.len(), 100_005, "full output, not the 16 KiB tail");
        assert!(text.starts_with("aaaaa"), "NUL bytes become 'a'");
        assert!(text.ends_with("\nend\n"), "tail marker intact");

        // Unknown task id in a known session → 404.
        let unknown = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/sessions/{id}/tasks/999/output"))
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

        // A delegate task (no output spool) → 404.
        session
            .background
            .spawn_with_id(
                "delegate task".into(),
                None,
                None,
                None,
                crate::tools::new_exit_slot(),
                |_| {},
                || async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    "done".into()
                },
            )
            .expect("delegate background task starts");
        let delegate = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/sessions/{id}/tasks/2/output"))
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(delegate.status(), StatusCode::NOT_FOUND);

        // A subagent session id → 404 (its tasks live in the parent
        // session's registry).
        let (handle, _emitter, _commands) = crate::runner::session_test_channel();
        let entry = Arc::new(crate::delegate::SessionEntry {
            handle,
            model: "sub-model".into(),
            role: None,
            cwd: "/tmp".into(),
            session_id: "sub-out".into(),
            context_window: None,
            store: SessionStore::Jsonl,
        });
        session.sessions.insert(7, entry);
        let subagent = app
            .oneshot(
                Request::builder()
                    .uri("/api/sessions/sub-out/tasks/1/output")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(subagent.status(), StatusCode::NOT_FOUND);

        // Clean up the running tasks so the test leaks no processes.
        session.background.cancel(1);
        session.background.cancel(2);
    }

    #[tokio::test]
    async fn pet_endpoints_report_config_and_serve_only_allowed_sheet() {
        use axum::http::Request;
        use tower::util::ServiceExt;

        let temp = tempfile::tempdir().unwrap();
        let sheet = temp.path().join("maid.webp");
        std::fs::write(&sheet, b"fake-webp").unwrap();
        let config: crate::config::Config = toml::from_str(&format!(
            "[pet]\nspritesheet = {:?}\nsprite_cols = 8\nsprite_rows = 11\nframe_width = 192\nframe_height = 208\nidle_row = 3\nidle_frames = 5\nloop_ms = 1200\n",
            sheet
        ))
        .unwrap();
        let mut state = test_app_state("sekrit");
        Arc::get_mut(&mut state).unwrap().factory =
            crate::session_factory::SessionFactory::test_factory_with_config(
                temp.path().to_path_buf(),
                Some(config),
            );
        let app = router(state);

        let runtime = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/pet/config")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(runtime.status(), StatusCode::OK);
        let body = axum::body::to_bytes(runtime.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "enabled": true, "cols": 8, "rows": 11,
                "frame_width": 192, "frame_height": 208,
                "idle_row": 3, "idle_frames": 5, "loop_ms": 1200
            })
        );

        let sprite = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/pet/sprite?token=sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(sprite.status(), StatusCode::OK);
        assert_eq!(
            sprite.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/webp"
        );
        assert_eq!(
            &axum::body::to_bytes(sprite.into_body(), 1024)
                .await
                .unwrap()[..],
            b"fake-webp"
        );
        let unauthorized = app
            .oneshot(
                Request::builder()
                    .uri("/api/pet/sprite")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let unset = router(test_app_state("sekrit"));
        let response = unset
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/pet/sprite?token=sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let runtime = unset
            .oneshot(
                Request::builder()
                    .uri("/api/pet/config?token=sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(runtime.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["enabled"], false);
    }

    #[tokio::test]
    async fn pet_endpoint_defaults_to_first_full_row_idle() {
        use axum::http::Request;
        use tower::util::ServiceExt;

        let temp = tempfile::tempdir().unwrap();
        let sheet = temp.path().join("maid.webp");
        std::fs::write(&sheet, b"fake-webp").unwrap();
        let config: crate::config::Config = toml::from_str(&format!(
            "[pet]\nspritesheet = {:?}\nsprite_cols = 8\nsprite_rows = 11\n",
            sheet
        ))
        .unwrap();
        let mut state = test_app_state("sekrit");
        Arc::get_mut(&mut state).unwrap().factory =
            crate::session_factory::SessionFactory::test_factory_with_config(
                temp.path().to_path_buf(),
                Some(config),
            );

        let runtime = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/pet/config")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(runtime.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["idle_row"], 0);
        assert_eq!(value["idle_frames"], 6);
        assert_eq!(value["loop_ms"], 1200);
    }

    #[tokio::test]
    async fn pet_endpoint_clamps_idle_values_to_sheet_grid() {
        use axum::http::Request;
        use tower::util::ServiceExt;

        async fn get(toml_body: String) -> serde_json::Value {
            let temp = tempfile::tempdir().unwrap();
            let sheet = temp.path().join("maid.webp");
            std::fs::write(&sheet, b"fake-webp").unwrap();
            let config: crate::config::Config =
                toml::from_str(&format!("[pet]\nspritesheet = {:?}\n{toml_body}", sheet)).unwrap();
            let mut state = test_app_state("sekrit");
            Arc::get_mut(&mut state).unwrap().factory =
                crate::session_factory::SessionFactory::test_factory_with_config(
                    temp.path().to_path_buf(),
                    Some(config),
                );
            let runtime = router(state)
                .oneshot(
                    Request::builder()
                        .uri("/api/pet/config")
                        .header(header::AUTHORIZATION, "Bearer sekrit")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let body = axum::body::to_bytes(runtime.into_body(), 1024 * 1024)
                .await
                .unwrap();
            serde_json::from_slice(&body).unwrap()
        }

        for (toml_body, idle_row, idle_frames, cols, rows) in [
            // idle_row beyond the grid clamps to the last row; idle_frames
            // beyond cols clamps to cols.
            (
                "sprite_cols = 4\nsprite_rows = 3\nidle_row = 9\nidle_frames = 7\n",
                2,
                4,
                4,
                3,
            ),
            // idle_frames = 0 clamps up to 1; single-cell grid stays safe.
            (
                "sprite_cols = 1\nsprite_rows = 1\nidle_row = 5\nidle_frames = 0\n",
                0,
                1,
                1,
                1,
            ),
            // Degenerate 0x0 grid normalizes to 1x1 before any rows - 1 math.
            (
                "sprite_cols = 0\nsprite_rows = 0\nidle_row = 2\nidle_frames = 3\n",
                0,
                1,
                1,
                1,
            ),
            // In-range values pass through untouched.
            (
                "sprite_cols = 8\nsprite_rows = 11\nidle_row = 4\nidle_frames = 3\n",
                4,
                3,
                8,
                11,
            ),
        ] {
            let value = get(toml_body.to_owned()).await;
            assert_eq!(
                (
                    value["idle_row"].as_u64().unwrap(),
                    value["idle_frames"].as_u64().unwrap(),
                    value["cols"].as_u64().unwrap(),
                    value["rows"].as_u64().unwrap()
                ),
                (idle_row, idle_frames, cols, rows),
                "case: {toml_body}"
            );
        }
    }

    #[tokio::test]
    async fn pet_sprite_rejects_extension_and_size_cap() {
        use axum::http::Request;
        use tower::util::ServiceExt;

        for (name, bytes) in [
            ("maid.svg", vec![b'x'; 10]),
            ("maid.png", vec![b'x'; PET_SPRITE_MAX_BYTES as usize + 1]),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let sheet = temp.path().join(name);
            std::fs::write(&sheet, bytes).unwrap();
            let config: crate::config::Config =
                toml::from_str(&format!("[pet]\nspritesheet = {:?}\n", sheet)).unwrap();
            let mut state = test_app_state("sekrit");
            Arc::get_mut(&mut state).unwrap().factory =
                crate::session_factory::SessionFactory::test_factory_with_config(
                    temp.path().to_path_buf(),
                    Some(config),
                );
            let response = router(state)
                .oneshot(
                    Request::builder()
                        .uri("/api/pet/sprite?token=sekrit")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{name}");
        }
    }

    /// `GET /api/images/{hash}`：内容寻址图片存取。测试用临时目录构造
    /// image store——`image_store_dir()` 读 `XDG_STATE_HOME` 环境变量，
    /// 测试把它指向临时目录（roles/tests.rs 同款 unsafe set_var 惯例；
    /// 无其它测试读该变量，无并行干扰），再按 `store_image_bytes` 的
    /// 布局（`<xdg>/e-agent/images/<hash>`）写入图片字节。
    #[tokio::test]
    async fn serve_image_serves_stored_bytes_and_validates_hash() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        let dir =
            std::env::temp_dir().join(format!("e-agent-server-images-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = dir.join("e-agent/images");
        std::fs::create_dir_all(&store).unwrap();
        unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };

        let bytes = b"fake-png-bytes";
        let hash = crate::agent::image_sha256(bytes);
        std::fs::write(store.join(&hash), bytes).unwrap();

        let app = router(test_app_state("sekrit"));

        // 无 token → 401（require_auth 对图片端点同样生效）。
        let noauth = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/images/{hash}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(noauth.status(), StatusCode::UNAUTHORIZED);

        // 文件存在：200 + 白名单 mime + 字节内容（?token= 供 <img> 使用）。
        let ok = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/images/{hash}?mime=image/png&token=sekrit"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        assert_eq!(
            ok.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("image/png")
        );
        let body = axum::body::to_bytes(ok.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], bytes);

        // 白名单之外的 mime → octet-stream 兜底（header 注入面为零）。
        let odd = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/images/{hash}?mime=text/html&token=sekrit"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(odd.status(), StatusCode::OK);
        assert_eq!(
            odd.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/octet-stream")
        );

        // 非法 hash 格式（非 64 位小写 hex，含路径穿越串）→ 400：直接调
        // handler 验证校验逻辑，不经 URL 规范化（`..` 段在真实请求里会被
        // 路由层先行处理，这里只关心 hash 校验本身）。
        let bad_hashes: [String; 6] = [
            String::new(),
            "abc".into(),
            "deadbeef".into(),
            "a".repeat(63),
            "A".repeat(64),
            "../etc/passwd".into(),
        ];
        for bad in bad_hashes {
            let result = serve_image(Path(bad.clone()), Query(ImageParams { mime: None })).await;
            let Err((status, _)) = result else {
                panic!("hash {bad:?} must be rejected");
            };
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "hash {bad:?} must be rejected"
            );
        }

        // 合法格式（64 位小写 hex）但文件不存在 → 404。
        let missing = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/images/{}?token=sekrit", "0".repeat(64)))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);

        unsafe { std::env::remove_var("XDG_STATE_HOME") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `GET /api/tasks` lists running bash and delegate tasks across
    /// sessions with the wire fields (sorted by `(session_id, id)`), and
    /// `DELETE /api/sessions/{id}/tasks/{task_id}` cancels one (204, then
    /// the task is gone; a second cancel 404s).
    #[tokio::test]
    async fn list_tasks_and_cancel_running_tasks() {
        use axum::body::Body;
        use axum::http::Request;
        use std::time::Duration;
        use tower::util::ServiceExt;

        let state = test_app_state("sekrit");
        let (id_a, session_a, _rx_a) = live_session_with_background_sender("web-a");
        let (id_b, session_b, _rx_b) = live_session_with_background_sender("web-b");
        state.registry.insert(id_a.clone(), session_a.clone());
        state.registry.insert(id_b.clone(), session_b.clone());

        // A real background bash task. The test registry is sender-equipped
        // and sandbox-less; the command outlives the assertions (30s) so
        // `running()` stays stable, and the test cancels it before exiting.
        let workspace = crate::workspace::Workspace::new(std::env::temp_dir()).unwrap();
        session_a
            .background
            .start(workspace.clone(), "sleep 30".to_string(), false)
            .expect("bash background task starts");
        // A delegate task with display metadata (background + workspace).
        session_b
            .background
            .spawn_with_id(
                "delegate task".into(),
                Some("coder".into()),
                None,
                Some(TaskDisplayMeta {
                    background: true,
                    workspace: Some("/tmp/dw".into()),
                    subagent_session_id: None,
                    resume: None,
                }),
                crate::tools::new_exit_slot(),
                |_| {},
                || async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    "done".into()
                },
            )
            .expect("delegate background task starts");

        let app = router(state);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/tasks")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let tasks: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            tasks.len(),
            2,
            "one bash + one delegate task across two sessions: {tasks:?}"
        );
        // Sorted by (session_id, id): web-a before web-b.
        assert_eq!(tasks[0]["session_id"], "web-a");
        assert_eq!(tasks[1]["session_id"], "web-b");
        let bash = &tasks[0];
        assert_eq!(bash["id"], 1);
        assert_eq!(bash["kind"], "bash");
        assert_eq!(bash["label"], "sleep 30");
        assert_eq!(bash["full_command"], "sleep 30");
        assert_eq!(bash["output"], "");
        assert_eq!(bash["background"], false);
        assert_eq!(bash["workspace"], serde_json::Value::Null);
        assert_eq!(
            bash["owner_session"], "web-a",
            "unknown owner falls back to the registry session id"
        );
        let delegate = &tasks[1];
        assert_eq!(delegate["id"], 1);
        assert_eq!(delegate["kind"], "delegate");
        assert_eq!(delegate["label"], "delegate task");
        assert_eq!(delegate["full_command"], serde_json::Value::Null);
        assert_eq!(delegate["role"], "coder");
        assert_eq!(delegate["background"], true);
        assert_eq!(delegate["workspace"], "/tmp/dw");
        assert_eq!(delegate["resume"], serde_json::Value::Null);
        assert_eq!(
            delegate["owner_session"], "web-b",
            "delegate tasks have no separate owner; falls back to the parent session"
        );

        // A subagent's bash task carries its own session id as the owner:
        // the wire exposes it (the registry owner stays the parent). The
        // test registry's own sender is not reachable (private field on an
        // Arc), so pass a placeholder — the task is cancelled before it
        // completes, and completion delivery is not under test here.
        let (spare_tx, _spare_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::agent::AgentEvent>();
        session_a
            .background
            .start_with_sender(
                workspace,
                "sleep 30".to_string(),
                false,
                Some(spare_tx),
                None,
                Some("sub-a1".into()),
            )
            .expect("subagent bash background task starts");
        let subagent_bash = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/tasks")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(subagent_bash.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let tasks: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
        let owned = tasks
            .iter()
            .find(|task| task["id"] == 2 && task["session_id"] == "web-a")
            .expect("subagent-owned bash task is listed");
        assert_eq!(owned["kind"], "bash");
        assert_eq!(owned["session_id"], "web-a", "registry owner is the parent");
        assert_eq!(
            owned["owner_session"], "sub-a1",
            "initiator is the subagent"
        );
        // Cancel the subagent-owned task so the later `running().is_empty()`
        // assertion (after cancelling task 1) stays exact.
        session_a.background.cancel(2);

        // Cancel the bash task via the endpoint: 204, task gone, and a
        // second cancel 404s.
        let cancel = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/sessions/{id_a}/tasks/1"))
                    .method("DELETE")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cancel.status(), StatusCode::NO_CONTENT);
        assert!(
            session_a.background.running().is_empty(),
            "bash task must be cancelled"
        );
        let cancel_twice = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/sessions/{id_a}/tasks/1"))
                    .method("DELETE")
                    .header(header::AUTHORIZATION, "Bearer sekrit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cancel_twice.status(), StatusCode::NOT_FOUND);

        // Clean up the delegate task too (it is still registered).
        assert!(
            session_b.background.cancel(1).is_some(),
            "delegate task is cancelled"
        );
    }
}
