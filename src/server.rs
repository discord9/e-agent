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
//! | POST   | `/api/sessions/{id}/prompt`       | queue a prompt                     |
//! | POST   | `/api/sessions/{id}/cancel`       | cancel the in-flight turn          |
//! | POST   | `/api/sessions/{id}/compact`      | request compaction                 |
//! | DELETE | `/api/sessions/{id}`              | cancel + remove from the registry  |
//! | GET    | `/api/tasks`                        | running background tasks, all sessions |
//! | DELETE | `/api/sessions/{id}/tasks/{task_id}` | cancel one background task          |
//!
//! Authentication: a random token generated at startup (written to
//! `$XDG_STATE_HOME/e-agent/server.token` or `~/.local/state/e-agent/server.token`,
//! mode 0600) is required on every `/api/*` request as
//! `Authorization: Bearer <token>` or `?token=<token>`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};

use anyhow::{Context as AnyhowContext, anyhow};
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Error, Json, Router};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc, watch};

use crate::agent::{AgentEvent, SessionEntry};
use crate::delegate::Sessions;
use crate::runner::{IdlePolicy, SessionHandle, SessionStatus, SessionTask};
use crate::session_factory::{SessionBuild, SessionFactory};
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
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;
    Ok(())
}

async fn shutdown_signal() {
    // Ctrl-C only. A second Ctrl-C kills the process outright (default
    // handler) while the first is draining in-flight connections.
    let _ = tokio::signal::ctrl_c().await;
}

/// Everything the handlers need: the shared factory, the live-session
/// registry, and the startup token.
pub struct AppState {
    pub factory: SessionFactory,
    pub registry: Arc<SessionRegistry>,
    pub token: String,
    /// Workspace-scoped sessions-metadata store: historical sessions for
    /// `GET /api/sessions` (Greptime) and `delete_meta` hiding. The Jsonl
    /// variant is the registry-only marker (list is always empty).
    pub meta_store: SessionStore,
}

fn router(state: Arc<AppState>) -> Router {
    let api = Router::new()
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/{id}/events", get(session_events))
        .route("/api/sessions/{id}/history", get(session_history))
        .route("/api/sessions/{id}/prompt", post(session_prompt))
        .route("/api/sessions/{id}/cancel", post(session_cancel))
        .route("/api/sessions/{id}/compact", post(session_compact))
        .route("/api/sessions/{id}", delete(delete_session))
        .route("/api/tasks", get(list_tasks))
        .route("/api/sessions/{id}/tasks/{task_id}", delete(cancel_task))
        .route_layer(from_fn_with_state(state.clone(), require_auth));
    Router::new()
        .route("/", get(index))
        .merge(api)
        .with_state(state)
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
        std::env::var_os("HOME").as_deref(),
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
    pub model_name: String,
    pub role_name: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
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
        model: session.model_name.clone(),
        role: session.role_name.clone(),
        created_at: session.created_at,
        status: status_string(&status).to_owned(),
        entry_count,
        busy: matches!(status, SessionStatus::Busy | SessionStatus::Compacting),
        active: true,
        parent_session_id: None,
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
        by_id.entry(id).or_insert_with(|| SessionMeta {
            id: meta.session_id,
            model: meta.model.unwrap_or_default(),
            role: meta.role,
            created_at: chrono::DateTime::from_naive_utc_and_offset(meta.created_at, chrono::Utc),
            // Historical sessions are not running; "Idle" renders as the
            // resumable chip and matches what a fresh resume shows.
            status: "Idle".to_owned(),
            entry_count: meta.entry_count.max(0) as usize,
            busy: false,
            active: false,
            parent_session_id: meta.parent_session_id,
        });
    }
    let mut metas: Vec<SessionMeta> = by_id.into_values().collect();
    metas.sort_by_key(|meta| meta.created_at);
    metas
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
    Json(merge_session_metas(active, historical))
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
        model_name: built.model_name,
        role_name: built.role_name,
        created_at: chrono::Utc::now(),
    });
    state.registry.insert(id.clone(), session.clone());
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
    factory
        .build(id, None, None, IdlePolicy::WaitForInput)
        .await
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))
}

fn live(state: &AppState, id: &str) -> Result<Arc<LiveSession>, (StatusCode, String)> {
    state
        .registry
        .get(id)
        .ok_or_else(|| error(StatusCode::NOT_FOUND, format!("session {id} not found")))
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
    let status = session.handle.status();
    if matches!(*status.borrow(), SessionStatus::Finished(_)) {
        return Err(error(
            StatusCode::CONFLICT,
            format!("session {id} has finished"),
        ));
    }
    session.handle.prompt(body.prompt);
    Ok(StatusCode::ACCEPTED)
}

async fn session_cancel(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let session = live(&state, &id)?;
    session.handle.cancel();
    Ok(StatusCode::ACCEPTED)
}

async fn session_compact(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let session = live(&state, &id)?;
    session.handle.compact();
    Ok(StatusCode::ACCEPTED)
}

async fn delete_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    // Live session: cancel + remove from the registry. Dropping the
    // registry entry (and with it the SessionTask) aborts the runner;
    // in-flight SSE streams notice the closed event/status channels and
    // end themselves. Unknown/historical ids simply skip this step.
    if let Some(session) = state.registry.get(&id) {
        session.handle.cancel();
        state.registry.remove(&id);
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
/// order (the registry is a HashMap).
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
    match session.background.cancel(task_id) {
        Some(_) => Ok(StatusCode::NO_CONTENT),
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
/// `next_before_seq` to page one compaction segment further back. `limit`
/// caps the number of entries returned (the newest are kept).
#[derive(Deserialize)]
pub struct HistoryParams {
    pub before_seq: Option<i64>,
    pub limit: Option<usize>,
}

/// One history page: a compaction segment of [`SessionEntry`] values plus
/// the cursor for the next older page. `next_before_seq` is `Some` when an
/// older segment exists (feed it back as `before_seq`), `None` when this
/// was the oldest segment or the session has no compaction at all.
#[derive(Serialize)]
pub struct HistoryResponse {
    pub entries: Vec<SessionEntry>,
    pub next_before_seq: Option<i64>,
}

/// Cap entries to the newest `limit` (oldest dropped); `None` = no cap.
/// Kept as a separate function so the wire shape and the cap are each
/// unit-testable without a session backend. Dropped entries inside one
/// segment are not paged individually: segments are compaction-bounded on
/// Greptime, and JSONL sessions have no older pages (`head_seq`/`load_older`
/// are both empty there), so the cap only ever trims the initial render.
fn cap_entries(entries: Vec<SessionEntry>, limit: Option<usize>) -> Vec<SessionEntry> {
    match limit {
        Some(limit) if entries.len() > limit => entries[entries.len() - limit..].to_vec(),
        _ => entries,
    }
}

/// `GET /api/sessions/{id}/history` — the frontend's initial-render path.
/// Returns one compaction segment of the session log: the head segment
/// (last compaction + everything after) without `before_seq`, or the
/// segment immediately older than `before_seq` when paging. `next_before_seq`
/// is the compaction seq that opens the returned segment (feed it back to
/// page further back); `null` means the oldest segment or no compaction.
async fn session_history(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<HistoryParams>,
) -> Result<Json<HistoryResponse>, (StatusCode, String)> {
    let session = live(&state, &id)?;
    let root = state.factory.root();
    let (entries, next_before_seq) = match params.before_seq {
        None => {
            // Head segment; the cursor is the seq of the compaction that
            // opens it (None = the whole session is one head segment).
            let loaded = session
                .store
                .load_head(root, &id)
                .await
                .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            let cursor = session
                .store
                .head_seq(root, &id)
                .await
                .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            (loaded.entries, cursor)
        }
        Some(before_seq) => {
            // Older segment: [prev_comp, before_seq); cursor = prev_comp.
            let (entries, cursor) = session
                .store
                .load_older(root, &id, before_seq)
                .await
                .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            (entries, cursor)
        }
    };
    Ok(Json(HistoryResponse {
        entries: cap_entries(entries, params.limit),
        next_before_seq,
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
    let (snapshot, live, status) = session.handle.attach();
    let (tx, rx) = mpsc::channel::<Result<Event, Error>>(SSE_CHANNEL_CAPACITY);
    tokio::spawn(forward_events(
        state.registry.clone(),
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
    registry: Arc<SessionRegistry>,
    id: String,
    snapshot: Vec<AgentEvent>,
    mut live: broadcast::Receiver<AgentEvent>,
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
            event = live.recv() => match event {
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
                    // with margin). A deleted session ends the stream.
                    let Some(session) = registry.get(&id) else { return };
                    let (snapshot, new_live, new_status) = session.handle.attach();
                    if !send(resync_event(&tail_snapshot(snapshot))) {
                        return;
                    }
                    if !send(status_event(&new_status.borrow().clone())) {
                        return;
                    }
                    live = new_live;
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
    // Dev-friendly: read the single-file UI from disk on every request so
    // frontend edits show up on refresh without recompiling. Located via
    // CARGO_MANIFEST_DIR (stable regardless of the process cwd).
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui/index.html");
    match std::fs::read_to_string(&path) {
        Ok(html) => HttpResponse::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            // Never cache the dev UI: browsers would serve a stale HTML
            // (missing the latest scroll/flex fixes) after a reload.
            .header(header::CACHE_CONTROL, "no-store")
            .body(html)
            .unwrap(),
        Err(error) => HttpResponse::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(format!(
                "<!doctype html><meta charset=\"utf-8\"><title>e-agent UI error</title>\
                 <body style=\"font-family:system-ui,sans-serif;padding:2rem\">\
                 <h1>Cannot read {}</h1><pre>{error}</pre></body>",
                path.display()
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
        let dir = std::env::temp_dir()
            .join(format!("e-agent-server-test-reuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server.token");
        let first = load_or_create_token_at(path.clone()).unwrap();
        let second = load_or_create_token_at(path.clone()).unwrap();
        assert_eq!(first, second, "existing token must be reused, not regenerated");
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
            model_name: "test-model".into(),
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
                model_name: "test-model".into(),
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
        assert_eq!(registry.get(&id).unwrap().model_name, "test-model");
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
            "status",
            "entry_count",
            "busy",
            "active",
            "parent_session_id",
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
            status: "Idle".into(),
            entry_count: 1,
            busy: false,
            active: false,
            parent_session_id: None,
        };
        let history = |id: &str, created: i64, count: i64| HistoryMeta {
            session_id: id.to_owned(),
            created_at: naive(created),
            last_active_at: naive(created + 10),
            model: None,
            role: None,
            entry_count: count,
            parent_session_id: None,
            parent_task_id: None,
        };
        // One registry session plus two historical sessions, one of which
        // overlaps a registry id (the registry entry wins).
        let merged = merge_session_metas(
            vec![wire("web-1", 200)],
            vec![history("tui-1", 50, 7), history("web-1", 100, 3)],
        );
        assert_eq!(merged.len(), 2);
        let web1 = merged.iter().find(|m| m.id == "web-1").unwrap();
        assert!(web1.active, "registry session stays active");
        assert_eq!(web1.entry_count, 1, "registry data wins over history");
        assert_eq!(web1.created_at, dt(200));
        let tui1 = merged.iter().find(|m| m.id == "tui-1").unwrap();
        assert!(!tui1.active, "historical session is inactive");
        assert_eq!(tui1.entry_count, 7);
        assert_eq!(tui1.status, "Idle", "historical sessions are resumable");
        assert_eq!(tui1.model, "", "history without model renders empty");
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

    #[test]
    fn history_limit_keeps_newest_entries() {
        use crate::agent::{Message, SessionEntry};
        let entry = |i: usize| SessionEntry::Message {
            message: Message::User {
                content: format!("m{i}"),
                images: vec![],
            },
        };
        let entries = vec![entry(1), entry(2), entry(3), entry(4)];
        assert_eq!(
            serde_json::to_value(cap_entries(entries.clone(), Some(2))).unwrap(),
            serde_json::json!([
                {"type": "message", "message": {"User": {"content": "m3"}}},
                {"type": "message", "message": {"User": {"content": "m4"}}},
            ])
        );
        // No cap or an oversized cap: unchanged.
        assert_eq!(cap_entries(entries.clone(), None).len(), 4);
        assert_eq!(cap_entries(entries, Some(99)).len(), 4);
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
        let registry = Arc::new(SessionRegistry::default());
        registry.insert(id.clone(), session.clone());

        // Subscribe, then overflow the broadcast buffer (capacity 256) while
        // the receiver idles so its next recv reports Lagged.
        let (snapshot, live, status) = handle.attach();
        for i in 0..300 {
            emitter.emit(AgentEvent::Notice(format!("n{i}")));
        }
        drop(emitter);

        let (tx, mut rx) = mpsc::channel::<Result<Event, Error>>(16);
        let task = tokio::spawn(forward_events(
            registry.clone(),
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
        registry.remove(&id);
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
        let registry = Arc::new(SessionRegistry::default());
        registry.insert(id.clone(), session.clone());
        let (snapshot, live, status) = handle.attach();

        // Capacity 1, already occupied: the very first try_send fails and
        // the stream ends.
        let (tx, _rx) = mpsc::channel::<Result<Event, Error>>(1);
        let _ = tx.try_send(Ok(Event::default().comment("prefill")));
        let task = tokio::spawn(forward_events(
            registry,
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
    /// is flattened into `background` + `workspace` (both empty for
    /// non-delegate tasks).
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
        // Delegate display metadata is flattened into background + workspace.
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
                }),
            },
        );
        assert_eq!(delegate.kind, "delegate");
        assert!(delegate.background);
        assert_eq!(delegate.workspace.as_deref(), Some("/tmp/w"));
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
