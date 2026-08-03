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
//! | POST   | `/api/sessions/{id}/prompt`       | queue a prompt                     |
//! | POST   | `/api/sessions/{id}/btw`          | fork into a persistent subagent    |
//! | GET    | `/api/sessions/{id}/fork-candidates` | turn boundaries to fork at (at/seq/preview) |
//! | POST   | `/api/sessions/{id}/fork`          | fork at a turn boundary into a `fork-…` session |
//! | POST   | `/api/sessions/{id}/cancel`       | cancel the in-flight turn          |
//! | POST   | `/api/sessions/{id}/compact`      | request compaction                 |
//! | GET    | `/api/models`                     | switchable model profile names (web `/model` autocomplete) |
//! | POST   | `/api/sessions/{id}/model`         | switch the session's model at runtime |
//! | POST   | `/api/sessions/{id}/undo`         | undo the most recent file operation |
//! | PUT    | `/api/sessions/{id}/title`        | rename a session (Greptime only)   |
//! | DELETE | `/api/sessions/{id}`              | cancel + remove from the registry  |
//! | GET    | `/api/tasks`                        | running background tasks, all sessions |
//! | DELETE | `/api/sessions/{id}/tasks/{task_id}` | cancel one background task          |
//! | GET    | `/api/sessions/{id}/tasks/{task_id}/output` | full output of a running bash task |
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
use axum::extract::{Path, Query, State};
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
use crate::session_store::SessionStore;
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

/// Bind, authenticate, and serve until Ctrl-C. The factory is resolved once
/// here and reused by every session `build()`.
pub async fn run(factory: SessionFactory, host: &str, port: u16) -> anyhow::Result<()> {
    let token = load_or_create_token()?;
    // Sessions-metadata store, connected once at bootstrap. Greptime:
    // create the audit table and run the one-time backfill of sessions
    // that predate it (L3: never inside connect). Jsonl: registry-only
    // listing marker (list_meta is empty).
    let meta_store = SessionStore::connect_meta(factory.backend(), factory.root()).await?;
    meta_store
        .backfill_sessions(factory.root())
        .await
        .context("cannot backfill session metadata")?;
    let state = Arc::new(AppState {
        factory,
        registry: Arc::new(SessionRegistry::default()),
        token,
        meta_store,
        summaries: Arc::new(Mutex::new(HashMap::new())),
        summary_pending: Arc::new(SummaryPending(Mutex::new(HashSet::new()))),
    });
    eprintln!(
        "e-agent: serving on http://{host}:{port} (token: {}; also at {})",
        state.token,
        token_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<no state dir>".to_owned())
    );
    let listener = tokio::net::TcpListener::bind((host, port))
        .await
        .with_context(|| format!("cannot bind {host}:{port}"))?;
    // Graceful shutdown on Ctrl-C, but bound: long-lived SSE connections
    // (a phone keeping the chat open) would otherwise hold the process
    // forever — the first Ctrl-C looks like it "does nothing". After
    // SHUTDOWN_DRAIN_TIMEOUT the server exits anyway (a second Ctrl-C
    // still kills outright via the default handler).
    let shutdown = async {
        shutdown_signal().await;
        tokio::time::sleep(SHUTDOWN_DRAIN_TIMEOUT).await;
    };
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
        .context("server error")?;
    Ok(())
}

/// How long graceful shutdown waits for in-flight connections to close
/// after Ctrl-C before exiting regardless.
const SHUTDOWN_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

async fn shutdown_signal() {
    // Ctrl-C only. A second Ctrl-C kills the process outright (default
    // handler) while the first is draining in-flight connections.
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
    /// Workspace-scoped sessions-metadata store: historical sessions for
    /// `GET /api/sessions` (Greptime) and `delete_meta` hiding. The Jsonl
    /// variant is the registry-only marker (list is always empty).
    pub meta_store: SessionStore,
    /// session_id -> (总结文本, 生成时间戳): written at the end of every
    /// turn with real activity, read by the desktop pet via
    /// `GET /api/sessions/{id}/summary`. Read-mostly, so a plain
    /// `Mutex<HashMap>` suffices.
    pub summaries: Arc<Mutex<HashMap<String, SummaryEntry>>>,
    /// session_id -> in-flight on-demand generation (desktop pet click on a
    /// cold cache): prevents duplicate generation calls.
    pub summary_pending: Arc<SummaryPending>,
}

fn router(state: Arc<AppState>) -> Router {
    let api = Router::new()
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/{id}/events", get(session_events))
        .route("/api/sessions/{id}/history", get(session_history))
        .route("/api/sessions/{id}/summary", get(session_summary))
        .route("/api/sessions/{id}/prompt", post(session_prompt))
        .route("/api/sessions/{id}/btw", post(session_btw))
        .route("/api/sessions/{id}/fork-candidates", get(fork_candidates))
        .route("/api/sessions/{id}/fork", post(session_fork))
        .route("/api/sessions/{id}/cancel", post(session_cancel))
        .route("/api/sessions/{id}/compact", post(session_compact))
        .route("/api/models", get(list_models))
        .route("/api/sessions/{id}/model", post(session_model))
        .route("/api/sessions/{id}/undo", post(session_undo))
        .route("/api/sessions/{id}/title", put(session_title))
        .route("/api/sessions/{id}/pin", put(session_pin))
        .route("/api/sessions/{id}/archive", put(session_archive))
        .route("/api/sessions/{id}", delete(delete_session))
        .route("/api/tasks", get(list_tasks))
        .route("/api/sessions/{id}/tasks/{task_id}", delete(cancel_task))
        .route(
            "/api/sessions/{id}/tasks/{task_id}/output",
            get(task_output),
        )
        .route_layer(from_fn_with_state(state.clone(), require_auth));
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
pub struct SessionRegistry {
    inner: Mutex<HashMap<String, Arc<LiveSession>>>,
}

impl SessionRegistry {
    pub fn insert(&self, id: String, session: Arc<LiveSession>) {
        self.inner.lock().unwrap().insert(id, session);
    }

    pub fn get(&self, id: &str) -> Option<Arc<LiveSession>> {
        self.inner.lock().unwrap().get(id).cloned()
    }

    pub fn remove(&self, id: &str) -> Option<Arc<LiveSession>> {
        self.inner.lock().unwrap().remove(id)
    }

    /// Snapshot of `(id, session)` pairs; the caller may hold them beyond a
    /// registry lock.
    pub fn list(&self) -> Vec<(String, Arc<LiveSession>)> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .map(|(id, session)| (id.clone(), session.clone()))
            .collect()
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
    /// True while the session is live in the registry. Historical sessions
    /// from the metadata table are `false` and rendered grey by the
    /// frontend; clicking one resumes it (`POST /api/sessions {id}`).
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
    let entry_count = session
        .store
        .load(root, id)
        .await
        .map(|loaded| loaded.entries.len())
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
}

/// Cap for `TaskMeta.output`. The per-task output tail is already capped at
/// 16 KiB in the spool; the wire DTO truncates further to this many
/// characters so a batch of running tasks stays a bounded `/api/tasks`
/// payload.
const TASK_OUTPUT_LIMIT: usize = 2000;

/// Map one [`BackgroundTaskInfo`] snapshot to its wire DTO. Pure function so
/// the field mapping (lossy UTF-8, truncation, display-meta flattening) is
/// unit-testable without a live task.
fn task_meta(session_id: &str, info: BackgroundTaskInfo) -> TaskMeta {
    let output = String::from_utf8_lossy(&info.output);
    let mut output = output.into_owned();
    if output.chars().count() > TASK_OUTPUT_LIMIT {
        output = output.chars().take(TASK_OUTPUT_LIMIT).collect();
    }
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

/// Apply one subagent's running_tasks label lookup to its list entry: a
/// surviving label means the delegate task that spawned it is still running
/// (rows are consumed when the task completes), so the subagent is live and
/// the entry must render in the live group. `None` = no live task, the
/// entry stays inactive. Pure so the active rule is unit-testable without
/// a Greptime backend.
fn apply_subagent_label(meta: &mut SessionMeta, label: Option<String>) {
    if label.is_some() {
        meta.active = true;
    }
    meta.label = label;
}

async fn list_sessions(State(state): State<Arc<AppState>>) -> Json<Vec<SessionMeta>> {
    let root = state.factory.root();
    let mut active: Vec<SessionMeta> = Vec::with_capacity(state.registry.list().len());
    for (id, session) in state.registry.list() {
        active.push(session_meta(&id, &session, root).await);
    }
    // Historical sessions from the metadata table (Greptime). The Jsonl
    // backend lists nothing — the registry is the whole list (M4).
    let historical = match state.meta_store.list_meta(root).await {
        Ok(list) => list,
        Err(error) => {
            eprintln!("e-agent: cannot list session metadata: {error:#}");
            Vec::new()
        }
    };
    let mut merged = merge_session_metas(active, historical);
    // Subagent items carry the task-panel label of their delegate task,
    // which lives in `running_tasks` (the sessions metadata table has no
    // label column). One lookup per subagent item — fine in practice
    // because only a handful of delegates run at once; a batched
    // `subagent_session_id = ANY(...)` query stays premature until that
    // assumption breaks.
    for meta in &mut merged {
        if meta.parent_session_id.is_some() {
            match state.meta_store.label_for_subagent(root, &meta.id).await {
                Ok(label) => {
                    // A surviving running_tasks row means the delegate task
                    // that spawned this subagent is still running (rows are
                    // consumed when the task completes) — so the subagent
                    // is live right now. Its id is not in the registry, so
                    // the merge would otherwise leave it grey/inactive;
                    // mark it active so the sidebar tree renders it in the
                    // live group (status stays as-is: running_tasks cannot
                    // tell Idle from Busy, and `active` alone is what the
                    // live grouping keys off).
                    apply_subagent_label(meta, label);
                }
                Err(error) => {
                    eprintln!("e-agent: cannot look up subagent label: {error:#}");
                }
            }
        }
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
    let id = match body.id {
        Some(id) => {
            crate::session::validate_session_name(&id)
                .map_err(|e| error(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            id
        }
        None => crate::session::new_id_prefixed("web-"),
    };
    if state.registry.get(&id).is_some() {
        return Err(error(
            StatusCode::CONFLICT,
            format!("session {id} already exists"),
        ));
    }
    let built = build_session(&state.factory, &id).await?;
    let initial_prompt = body.initial_prompt.filter(|p| !p.trim().is_empty());
    let session = Arc::new(LiveSession {
        handle: built.handle,
        task: built.runner.start(initial_prompt),
        store: built.store,
        background: built.background,
        sessions: built.sessions,
        model_name: Mutex::new(built.model_name),
        role_name: built.role_name,
        created_at: chrono::Utc::now(),
    });
    state.registry.insert(id.clone(), session.clone());
    // 桌宠总结：每个 turn（Busy→Idle）结束时后台生成一句话中文总结并缓存。
    // 监听任务订阅 runner 的 status watch（不改 runner.rs）；会话删除/运行器
    // 退出（watch sender drop）时自动结束。
    spawn_summary_listener(state.clone(), id.clone(), session.clone());
    let root = state.factory.root();
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
    // Lazy server attach: the session may still be live in another process,
    // so do NOT consume its unfinished-background records or inject a
    // "killed with the process" notice — the owning process clears them
    // itself via ack_background_entry → clear_background_task.
    factory
        .build(
            id,
            None,
            None,
            IdlePolicy::WaitForInput,
            UnfinishedPolicy::Preserve,
        )
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

/// `POST /api/sessions/{id}/model` — switch the session's model at runtime
/// (web `/model <profile>`). Body `{"profile": "provider/model"}`; the
/// profile is resolved against the same config the factory was built with
/// (honoring `--base-url`/`--model` overrides), then installed on the live
/// runner's agent — the session keeps its history and continues with the
/// new model. 200 + `{"ok": true, "model": "<display name>"}` on success;
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
    let name = configured.display_name().to_owned();
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
/// live nor present in the metadata table. The JSONL backend has no meta
/// table: renaming a live session is a silent no-op (title is
/// Greptime-only).
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
            let historical = state
                .meta_store
                .list_meta(root)
                .await
                .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            if !historical.iter().any(|m| m.session_id == id) {
                return Err(error(
                    StatusCode::NOT_FOUND,
                    format!("session {id} not found"),
                ));
            }
            state.meta_store.clone()
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
/// live nor present in the metadata table. The JSONL backend has no meta
/// table: pinning a live session is a silent no-op (pins are Greptime-only).
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
            let historical = state
                .meta_store
                .list_meta(root)
                .await
                .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            if !historical.iter().any(|m| m.session_id == id) {
                return Err(error(
                    StatusCode::NOT_FOUND,
                    format!("session {id} not found"),
                ));
            }
            state.meta_store.clone()
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
/// neither live nor present in the metadata table. The JSONL backend has
/// no meta table: archiving a live session is a silent no-op (archives
/// are Greptime/SQLite-only).
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
            let historical = state
                .meta_store
                .list_meta(root)
                .await
                .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            if !historical.iter().any(|m| m.session_id == id) {
                return Err(error(
                    StatusCode::NOT_FOUND,
                    format!("session {id} not found"),
                ));
            }
            state.meta_store.clone()
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
    // Live session: cancel + remove from the registry. Dropping the
    // registry entry (and with it the SessionTask) aborts the runner;
    // in-flight SSE streams notice the closed event/status channels and
    // end themselves.
    //
    // A live subagent is truly terminated through its parent's
    // `BackgroundTasks::cancel(task_id)` instead of a plain
    // `handle().cancel()`: the latter only interrupts the current turn and
    // the delegate runner now stays alive (Idle) for follow-up messages, so
    // it could never end the session on its own. The abort drops the
    // delegate wrapper, whose captured cleanup removes the `Sessions` entry
    // and the running_tasks row, and dropping the wrapper's runner handle
    // aborts the subagent runner (SessionTask::drop). This is the same path
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
    // (Greptime audit table; JSONL no-op). The transcript stays, so a
    // later resume still works.
    let root = state.factory.root();
    state
        .meta_store
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
    for (session_id, session) in state.registry.list() {
        for info in session.background.running() {
            tasks.push(task_meta(&session_id, info));
        }
    }
    tasks.sort_by(|a, b| (&a.session_id, a.id).cmp(&(&b.session_id, b.id)));
    Json(tasks)
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
/// polling honest. 404 for an unknown session, a subagent session (its
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
    // Live path: both variants carry a session-bound store — the live
    // registry session owns one, and a subagent's `SessionEntry` carries
    // its own (connected at spawn time) — so history reads the same rows
    // the session itself persists to, with no per-request store connect.
    // Historical fallback (registry miss): connect a store by id and read
    // the same rows the live path would; a truly unknown id leaves the
    // store empty → 404, and ids that can never exist (invalid session
    // name) also 404, keeping the previous registry-miss semantics.
    let store = resolve_session_store(&state, &id).await?;
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
            store
                .load_head_page(root, &id, params.limit)
                .await
                .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
        }
        Some(before_seq) => {
            // Older entries: [prev_comp, before_seq), paged intra-segment
            // by `limit` when present (cursor = oldest seq of the page,
            // crossing into the older segment at a compaction boundary).
            let (entries, cursor) = store
                .load_older(root, &id, before_seq, params.limit)
                .await
                .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
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
) -> Result<SessionStore, (StatusCode, String)> {
    let root = state.factory.root();
    match live(state, id) {
        Ok(SessionRef::Live(session)) => Ok(session.store.clone()),
        Ok(SessionRef::Subagent { entry, .. }) => Ok(entry.store.clone()),
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
            let loaded = store
                .load_head(root, id)
                .await
                .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            if loaded.entries.is_empty() {
                return Err(error(
                    StatusCode::NOT_FOUND,
                    format!("session {id} not found"),
                ));
            }
            Ok(store)
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
    let store = resolve_session_store(&state, &id).await?;
    let with_seq = store
        .load_with_seq(root, &id)
        .await
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
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
    // Resolve the source first (404 before any build work); the store is
    // only needed for the existence check — `build` re-connects by id.
    resolve_session_store(&state, &id).await?;
    let built = state
        .factory
        .build(
            &id,
            Some((id.clone(), Some(at))),
            None,
            IdlePolicy::WaitForInput,
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
    state.registry.insert(new_id.clone(), session);
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
    let Ok(Ok((assistant, _))) =
        tokio::time::timeout(SUMMARY_TIMEOUT, model.complete(&messages, &[], None)).await
    else {
        return; // 模型失败/超时：静默，不阻塞会话
    };
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
    tokio::spawn(forward_events(
        state,
        id,
        tail_snapshot(snapshot),
        live,
        status,
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
        AgentEvent::BackgroundCompleted { id, output, label }
        | AgentEvent::BackgroundCompletionNotice { id, output, label } => {
            json!({ "id": id, "output": output, "label": label })
        }
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
fn html_headers() -> [(header::HeaderName, header::HeaderValue); 1] {
    [(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/html; charset=utf-8"),
    )]
}

#[cfg(not(web_ui))]
async fn index(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    (
        html_headers(),
        PLACEHOLDER_HTML.replace("__TOKEN__", &state.token),
    )
}

#[cfg(web_ui)]
async fn index() -> impl IntoResponse {
    use axum::http::Response as HttpResponse;
    // Dev-friendly: read the UI skeleton from disk and inline the CSS/JS
    // pieces on every request, so frontend edits (style.css, app.js, vendor
    // libs) show up on refresh without recompiling. The response stays a
    // single self-contained HTML file. Located via CARGO_MANIFEST_DIR
    // (stable regardless of the process cwd).
    let ui = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui");
    let read = |name: &str| -> Result<String, String> {
        std::fs::read_to_string(ui.join(name))
            .map_err(|e| format!("cannot read {}: {e}", ui.join(name).display()))
    };
    let assembled = read("index.html").and_then(|skeleton| {
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
    });
    match assembled {
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
                label: None
            }),
            "BackgroundCompleted"
        );
        assert_eq!(
            name(&AgentEvent::BackgroundCompletionNotice {
                id: 1,
                output: "o".into(),
                label: None,
            }),
            "BackgroundCompletionNotice"
        );
        assert_eq!(
            name(&AgentEvent::Usage {
                context_input: 1,
                context_window: None,
                session: crate::agent::Usage {
                    input_tokens: 1,
                    output_tokens: 2
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
            }),
            json!({"id": 7, "output": "ok", "label": "cargo"})
        );
        assert_eq!(
            event_payload(&AgentEvent::BackgroundCompletionNotice {
                id: 7,
                output: "ok".into(),
                label: None,
            }),
            json!({"id": 7, "output": "ok", "label": null})
        );
        assert_eq!(
            event_payload(&AgentEvent::Usage {
                context_input: 1234,
                context_window: Some(4096),
                session: crate::agent::Usage {
                    input_tokens: 100,
                    output_tokens: 50
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
            },
            AgentEvent::Usage {
                context_input: 1,
                context_window: None,
                session: crate::agent::Usage {
                    input_tokens: 1,
                    output_tokens: 0,
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
        assert_eq!(value["model"], "flash", "display name short part");
        // The registry metadata mirrors the switch for `GET /api/sessions`.
        assert_eq!(session.model_name(), "flash");

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

    #[test]
    #[cfg(not(web_ui))]
    fn placeholder_contains_running_and_token() {
        let html = PLACEHOLDER_HTML.replace("__TOKEN__", "abc");
        assert!(html.contains("server running"));
        assert!(html.contains("abc"));
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
    /// never reach `factory.build()`. The meta store is the Jsonl marker
    /// (registry-only listing).
    fn test_app_state(token: &str) -> Arc<AppState> {
        Arc::new(AppState {
            factory: crate::session_factory::SessionFactory::test_factory(std::env::temp_dir()),
            registry: Arc::new(SessionRegistry::default()),
            token: token.to_owned(),
            meta_store: SessionStore::Jsonl,
            summaries: Arc::new(Mutex::new(HashMap::new())),
            summary_pending: Arc::new(SummaryPending(Mutex::new(HashSet::new()))),
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

    /// The active rule for subagent list entries: a surviving running_tasks
    /// label means the delegate task is still running, so the subagent is
    /// live and must render in the live group; `None` (task completed, row
    /// consumed) keeps the entry inactive.
    #[test]
    fn subagent_label_marks_live_subagent_active() {
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
            live.active,
            "a surviving running_tasks label means the subagent is live"
        );
        assert_eq!(live.label.as_deref(), Some("delegate task"));
        let mut done = meta(false);
        apply_subagent_label(&mut done, None);
        assert!(
            !done.active,
            "no live delegate task keeps the subagent inactive"
        );
        assert_eq!(done.label, None);
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
    /// it. A plain `handle().cancel()` only interrupts the current turn — a
    /// FinishWhenIdle delegate now returns to Idle and stays alive for
    /// follow-up messages — so the endpoint routes through the parent's
    /// `BackgroundTasks::cancel(task_id)` instead: the delegate wrapper is
    /// aborted, its captured cleanup removes the `Sessions` entry, and
    /// dropping the wrapper's runner handle aborts the subagent runner.
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
        let task = tokio::spawn(forward_events(
            state,
            id,
            tail_snapshot(snapshot),
            live,
            status,
            tx,
        ));
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("forward_events must end when the queue is full")
            .expect("forward_events task must not panic");
    }

    /// M5 contract (frontend): each live SSE frame must pair the CamelCase
    /// event name (`applyLiveEvent` switch) with the flat payload keys
    /// (`pickText` / per-event handlers) on the actual wire — a name/payload
    /// mismatch would silently break rendering.
    #[tokio::test]
    async fn live_event_wire_frames_match_frontend_contract() {
        use serde_json::json;
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
    /// is flattened into `background` + `workspace` + `subagent_session_id`
    /// + `resume` (all empty for non-delegate tasks).
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
            },
        );
        assert_eq!(long.session_id, "web-x");
        assert_eq!(long.id, 7);
        assert_eq!(long.label, "big");
        assert_eq!(long.full_command.as_deref(), Some("yes"));
        assert_eq!(long.role.as_deref(), Some("coder"));
        assert_eq!(long.kind, "bash");
        assert_eq!(long.output.chars().count(), TASK_OUTPUT_LIMIT);
        assert!(!long.background, "non-delegate tasks have no display meta");
        assert_eq!(long.workspace, None);
        assert_eq!(long.subagent_session_id, None);
        assert_eq!(long.resume, None);
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
            },
        );
        assert_eq!(delegate.kind, "delegate");
        assert!(delegate.background);
        assert_eq!(delegate.workspace.as_deref(), Some("/tmp/w"));
        assert_eq!(delegate.subagent_session_id.as_deref(), Some("sub-abc"));
        assert_eq!(delegate.resume.as_deref(), Some("sub-resume-1"));
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
    /// `Cache-Control: no-cache` (proving it is the full spool, not the
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
            Some("no-cache"),
            "polling endpoint must not be cached"
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
            .start(workspace, "sleep 30".to_string(), false)
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
        let delegate = &tasks[1];
        assert_eq!(delegate["id"], 1);
        assert_eq!(delegate["kind"], "delegate");
        assert_eq!(delegate["label"], "delegate task");
        assert_eq!(delegate["full_command"], serde_json::Value::Null);
        assert_eq!(delegate["role"], "coder");
        assert_eq!(delegate["background"], true);
        assert_eq!(delegate["workspace"], "/tmp/dw");
        assert_eq!(delegate["resume"], serde_json::Value::Null);

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
