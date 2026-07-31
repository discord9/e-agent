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
use axum::extract::{Path, State};
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

use crate::agent::AgentEvent;
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

/// Busy-state JSON carried by `event: status` SSE frames and by the session
/// metadata endpoints. Kept as a server-side DTO so the wire shape is
/// independent of the runner's internal enum.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum StatusDto {
    Idle,
    Busy,
    Compacting,
    Finished { result: SessionResultDto },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionResultDto {
    Completed { answer: Option<String> },
    Failed { error: String },
    Cancelled,
    Closed,
}

impl From<SessionStatus> for StatusDto {
    fn from(status: SessionStatus) -> Self {
        use crate::runner::SessionResult;
        match status {
            SessionStatus::Idle => StatusDto::Idle,
            SessionStatus::Busy => StatusDto::Busy,
            SessionStatus::Compacting => StatusDto::Compacting,
            SessionStatus::Finished(result) => StatusDto::Finished {
                result: match result {
                    SessionResult::Completed(answer) => SessionResultDto::Completed { answer },
                    SessionResult::Failed(error) => SessionResultDto::Failed { error },
                    SessionResult::Cancelled => SessionResultDto::Cancelled,
                    SessionResult::Closed => SessionResultDto::Closed,
                },
            },
        }
    }
}

/// Session metadata for `GET /api/sessions` and `POST /api/sessions`.
#[derive(Serialize)]
pub struct SessionMeta {
    pub id: String,
    pub model: String,
    pub role: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub status: StatusDto,
}

fn session_meta(id: &str, session: &LiveSession) -> SessionMeta {
    let status = session.handle.status();
    SessionMeta {
        id: id.to_owned(),
        model: session.model_name.clone(),
        role: session.role_name.clone(),
        created_at: session.created_at,
        status: StatusDto::from(status.borrow().clone()),
    }
}

fn error(status: StatusCode, message: impl Into<String>) -> (StatusCode, String) {
    (status, message.into())
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Handlers
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

async fn list_sessions(State(state): State<Arc<AppState>>) -> Json<Vec<SessionMeta>> {
    let mut metas: Vec<SessionMeta> = state
        .registry
        .list()
        .iter()
        .map(|(id, session)| session_meta(id, session))
        .collect();
    metas.sort_by_key(|meta| meta.created_at);
    Json(metas)
}

#[derive(Deserialize)]
struct CreateSessionBody {
    /// Optional caller-chosen id; defaults to a fresh `web-…` id.
    #[serde(default)]
    id: Option<String>,
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
    let session = Arc::new(LiveSession {
        handle: built.handle,
        task: built.runner.start(None),
        store: built.store,
        background: built.background,
        sessions: built.sessions,
        model_name: built.model_name,
        role_name: built.role_name,
        created_at: chrono::Utc::now(),
    });
    state.registry.insert(id.clone(), session.clone());
    Ok((StatusCode::CREATED, Json(session_meta(&id, &session))))
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
// SSE
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

/// `GET /api/sessions/{id}/events` — one `event: snapshot` carrying the full
/// event array, one initial `event: status`, then live `event: message`
/// frames plus `event: status` on state changes and a `: ping` comment every
/// 15s. If a client ever falls behind the broadcast buffer, a fresh
/// `event: snapshot` is re-sent (the frontend replaces its view) and the
/// stream continues from there.
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
                    if !send(message_event(&event)) {
                        return;
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    // Client fell behind the broadcast buffer; resync with a
                    // fresh snapshot. A deleted session ends the stream.
                    let Some(session) = registry.get(&id) else { return };
                    let (snapshot, new_live, new_status) = session.handle.attach();
                    if !send(snapshot_event(&snapshot)) {
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

fn message_event(event: &AgentEvent) -> Result<Event, Error> {
    Event::default().event("message").json_data(event)
}

fn status_event(status: &SessionStatus) -> Result<Event, Error> {
    Event::default()
        .event("status")
        .json_data(StatusDto::from(status.clone()))
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
    fn status_dto_wire_shape() {
        use crate::runner::SessionResult;
        assert_eq!(
            serde_json::to_value(StatusDto::from(SessionStatus::Idle)).unwrap(),
            serde_json::json!({"status": "idle"})
        );
        assert_eq!(
            serde_json::to_value(StatusDto::from(SessionStatus::Finished(
                SessionResult::Completed(Some("hi".into()))
            )))
            .unwrap(),
            serde_json::json!({"status": "finished", "result": {"type": "completed", "answer": "hi"}})
        );
        assert_eq!(
            serde_json::to_value(StatusDto::from(SessionStatus::Finished(
                SessionResult::Failed("boom".into())
            )))
            .unwrap(),
            serde_json::json!({"status": "finished", "result": {"type": "failed", "error": "boom"}})
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
        // by `status_dto_wire_shape` and `agent_events_serialize_tagged`
        // (axum 0.8's `Event` exposes no getters to assert the buffer on).
        assert!(snapshot_event(&[AgentEvent::Notice("hi".into())]).is_ok());
        assert!(message_event(&AgentEvent::AssistantText("x".into())).is_ok());
        assert!(status_event(&SessionStatus::Busy).is_ok());
    }
}
