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
use crate::tools::BackgroundTasks;

/// Heartbeat interval for SSE connections (comment line `: ping`).
const HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(15);

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Startup
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

/// Bind, authenticate, and serve until Ctrl-C. The factory is resolved once
/// here and reused by every session `build()`.
pub async fn run(factory: SessionFactory, host: &str, port: u16) -> anyhow::Result<()> {
    let token = load_or_create_token()?;
    let state = Arc::new(AppState {
        factory,
        registry: Arc::new(SessionRegistry::default()),
        token,
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

/// Generate a fresh random token and write it to the state dir with mode
/// 0600. A new token every start: the previous server's clients (if any)
/// are invalidated, which is fine because only one server can bind the port.
pub fn load_or_create_token() -> anyhow::Result<String> {
    let path = token_path()
        .ok_or_else(|| anyhow!("cannot resolve server token path: no XDG_STATE_HOME or HOME"))?;
    load_or_create_token_at(path)
}

fn load_or_create_token_at(path: PathBuf) -> anyhow::Result<String> {
    let dir = path.parent().expect("token path always has a parent");
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
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
    }
}

fn error(status: StatusCode, message: impl Into<String>) -> (StatusCode, String) {
    (status, message.into())
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Handlers
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

async fn list_sessions(State(state): State<Arc<AppState>>) -> Json<Vec<SessionMeta>> {
    let root = state.factory.root();
    let mut metas: Vec<SessionMeta> = Vec::with_capacity(state.registry.list().len());
    for (id, session) in state.registry.list() {
        metas.push(session_meta(&id, &session, root).await);
    }
    metas.sort_by_key(|meta| meta.created_at);
    Json(metas)
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
    let session = live(&state, &id)?;
    session.handle.cancel();
    // Dropping the registry entry (and with it the SessionTask) aborts the
    // runner; in-flight SSE streams notice the closed event/status channels
    // and end themselves.
    state.registry.remove(&id);
    Ok(StatusCode::NO_CONTENT)
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

/// `GET /api/sessions/{id}/events` — one `event: snapshot` carrying the full
/// event array, one initial `event: status`, then live frames named after
/// the [`AgentEvent`] variant (CamelCase: `UserPrompt`, `AssistantDelta`,
/// `ToolCall`, …) plus `event: status` on state changes and a `: ping`
/// comment every 15s. If a client ever falls behind the broadcast buffer,
/// a fresh `event: resync` with the full event array is re-sent (the
/// frontend force-replaces its transcript) and the stream continues.
async fn session_events(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Response, (StatusCode, String)> {
    let session = live(&state, &id)?;
    let (snapshot, live, status) = session.handle.attach();
    let (tx, rx) = mpsc::unbounded_channel::<Result<Event, Error>>();
    tokio::spawn(forward_events(
        state.registry.clone(),
        id,
        snapshot,
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

/// tokio's `UnboundedReceiver` does not implement `Stream` on its own; this
/// adapter (a few lines, no extra dependency) exposes `poll_recv`.
struct SseReceiver(mpsc::UnboundedReceiver<Result<Event, Error>>);

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
    tx: mpsc::UnboundedSender<Result<Event, Error>>,
) {
    let send = |event: Result<Event, Error>| tx.send(event).is_ok();
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
                    // never skips it — it force-replaces the transcript). A
                    // deleted session ends the stream.
                    let Some(session) = registry.get(&id) else { return };
                    let (snapshot, new_live, new_status) = session.handle.attach();
                    if !send(resync_event(&snapshot)) {
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

/// Lag resync: the full event log re-sent when a client falls behind the
/// broadcast buffer. The frontend's `resync` branch force-replaces the
/// transcript (unlike `snapshot`, which it skips once history rendered).
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
            session,
        } => {
            json!({ "context_input": context_input, "session": session })
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
    (html_headers(), include_str!("ui/index.html"))
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Tests
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

#[cfg(test)]
mod tests {
    use super::*;

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
                session: crate::agent::Usage {
                    input_tokens: 100,
                    output_tokens: 50
                },
            }),
            json!({"context_input": 1234, "session": {"input_tokens": 100, "output_tokens": 50}})
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
        let workspace = crate::workspace::Workspace::new(std::env::temp_dir()).unwrap();
        let (_tools, background) = crate::tools::builtins(workspace, None, false);
        let live = LiveSession {
            handle,
            task: crate::runner::SessionTask::from_join_handle(tokio::spawn(async {})),
            store: SessionStore::Jsonl,
            background,
            sessions: Sessions::default(),
            model_name: "test-model".into(),
            role_name: None,
            created_at: chrono::Utc::now(),
        };
        (id.to_owned(), Arc::new(live))
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
    /// CamelCase string plus `entry_count` and `busy` (index.html reads
    /// `s.id`, `s.status`, `s.model`, `s.created_at`, `s.entry_count`,
    /// `s.busy`).
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
        ] {
            assert!(value.get(key).is_some(), "missing field {key}");
        }
        assert_eq!(value["status"], "Idle");
        assert_eq!(value["busy"], false);
        assert_eq!(value["entry_count"], 0);
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
}
