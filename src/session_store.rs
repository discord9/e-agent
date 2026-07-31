//! Runtime session backend dispatch. A simple enum chooses between the
//! default JSONL file backend and the optional GreptimeDB backend without
//! introducing a trait (see AGENTS.md: "one adapter, no seam").
//!
//! Each variant holds whatever state its backend needs:
//!
//! - **Jsonl** — stateless marker; every call provides `root` + `name`.
//! - **Greptime** — a connected + session-bound client behind a Mutex so
//!   `&self` methods work everywhere (including the delegate's closure-based
//!   the runner persistence path).
//!
//! Background-task state (`*.background.jsonl` files on JSONL, the
//! `running_tasks` table on Greptime) is dispatched through the same enum:
//! [`SessionStore::record_background_start`] /
//! [`SessionStore::clear_background_task`] /
//! [`SessionStore::take_unfinished_background`].

use std::path::{Path, PathBuf};
#[cfg(feature = "greptime")]
use std::sync::Arc;
#[cfg(feature = "greptime")]
use tokio::sync::Mutex;

use anyhow::Result;

use crate::agent::SessionEntry;
use crate::config::SessionBackend;
use crate::session::{LoadedSession, Session};

#[derive(Clone)]
pub enum SessionStore {
    /// Default file-based JSONL backend. Stateless — delegates to
    /// `session::Session` static methods.
    Jsonl,
    /// GreptimeDB-backed session storage behind a mutex so `&self` methods
    /// work from any context.
    #[cfg(feature = "greptime")]
    Greptime {
        /// The connected Greptime session client, session-bound.
        session: Arc<Mutex<crate::session_greptime::GreptimeSession>>,
        /// Connection string, preserved so the backend config can be
        /// recovered for subagent session binding.
        conn: String,
    },
}

/// Where a live agent records its in-flight background tasks: the workspace
/// root, the session name the record belongs to, and the store that owns
/// the record. Carried by the main agent (`agent.background_record`) and
/// the delegate tool (`delegate.record_in`) so background bash and delegate
/// tasks are reported as "killed on exit" when their session is resumed.
#[derive(Clone)]
pub struct BackgroundRecord {
    pub root: PathBuf,
    pub session: String,
    pub store: SessionStore,
}

impl SessionStore {
    /// Create a new store based on the configured backend.
    ///
    /// For `Jsonl` this is a zero-cost marker; for `Greptime` it connects to
    /// the database and ensures the session table exists. The `session_id`
    /// and `workspace_id` are bound at connect time for Greptime (the
    /// backend is per-session).  The `workspace_id` is derived from the
    /// canonical workspace root to namespace sessions by workspace.
    ///
    /// When the `greptime` feature is not enabled and the config selects
    /// Greptime, returns an error explaining the missing feature.
    #[allow(unused_variables)]
    pub async fn connect(backend: &SessionBackend, root: &Path, session_id: &str) -> Result<Self> {
        match backend {
            SessionBackend::Jsonl => Ok(SessionStore::Jsonl),
            #[cfg(feature = "greptime")]
            SessionBackend::Greptime { conn } => {
                let workspace_id = crate::session_greptime::derive_workspace_id(root);
                let session = crate::session_greptime::GreptimeSession::connect(
                    conn,
                    &workspace_id,
                    session_id,
                )
                .await?;
                Ok(SessionStore::Greptime {
                    session: Arc::new(Mutex::new(session)),
                    conn: conn.clone(),
                })
            }
            #[cfg(not(feature = "greptime"))]
            SessionBackend::Greptime { .. } => {
                anyhow::bail!("greptime session backend requires the `greptime` cargo feature");
            }
        }
    }

    /// Return the backend configuration that was used to create this store.
    ///
    /// For JSONL returns `SessionBackend::Jsonl`; for Greptime returns
    /// `SessionBackend::Greptime { conn }` with the stored connection string.
    pub fn backend(&self) -> SessionBackend {
        match self {
            SessionStore::Jsonl => SessionBackend::Jsonl,
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { conn, .. } => SessionBackend::Greptime { conn: conn.clone() },
        }
    }

    /// Load session entries.
    ///
    /// For JSONL, `root` and `name` locate the file. For Greptime, the
    /// session is already bound so `root`/`name` are unused.
    pub async fn load(&self, root: &Path, name: &str) -> Result<LoadedSession> {
        match self {
            SessionStore::Jsonl => Session::load(root, name),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                let entries = { session.lock().await.load().await? };
                Ok(LoadedSession {
                    entries,
                    legacy: false,
                })
            }
        }
    }

    /// Load entries paired with their backend sequence number, in load
    /// order. Used by `--fork` to record the source entry's seq as
    /// provenance on the `ForkedFrom` marker.
    ///
    /// For JSONL there is no seq column; the 0-based ordinal (the JSONL line
    /// index) is returned so the provenance is still meaningful. For
    /// Greptime the real `seq` column values are returned.
    pub async fn load_with_seq(&self, root: &Path, name: &str) -> Result<Vec<(i64, SessionEntry)>> {
        match self {
            SessionStore::Jsonl => {
                let loaded = Session::load(root, name)?;
                Ok(loaded
                    .entries
                    .into_iter()
                    .enumerate()
                    .map(|(index, entry)| (index as i64, entry))
                    .collect())
            }
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => session.lock().await.load_with_seq().await,
        }
    }

    /// Append entries to the session log.
    ///
    /// For JSONL, `root` and `name` locate the file. For Greptime, the
    /// session is already bound so `root`/`name` are unused.
    pub async fn append(&self, root: &Path, name: &str, entries: &[SessionEntry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        match self {
            SessionStore::Jsonl => Session::append(root, name, entries),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => session.lock().await.append(entries).await,
        }
    }

    /// Rewrite the entire session log (used for legacy migration).
    ///
    /// For JSONL this replaces the file atomically. For Greptime it is a
    /// no-op — compaction is append-only.
    pub async fn rewrite(&self, root: &Path, name: &str, entries: &[SessionEntry]) -> Result<()> {
        match self {
            SessionStore::Jsonl => Session::rewrite(root, name, entries),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { .. } => Ok(()), // Append-only; rewriting is a no-op.
        }
    }

    // ------------------------------------------------------------------
    // Background-task state records
    // ------------------------------------------------------------------
    //
    // Sync facade for the record/clear paths: JSONL writes the record file
    // synchronously; Greptime schedules the write onto the current tokio
    // runtime (every production call site — tool completion, task ack,
    // delegate cleanup, TUI cancel — runs inside one). Errors are reported
    // on stderr: recording must never break the agent loop, matching the
    // `let _ =` callers of the old `Session::record_background_start`.

    /// Record a freshly started background task so a later launch can tell
    /// the user what died with the previous process.
    pub fn record_background_start(
        &self,
        root: &Path,
        session: &str,
        id: u64,
        label: &str,
        subagent_session_id: Option<&str>,
    ) {
        match self {
            SessionStore::Jsonl => {
                if let Err(error) =
                    Session::record_background_start(root, session, id, label, subagent_session_id)
                {
                    eprintln!("e-agent: cannot record background task: {error:#}");
                }
            }
            #[cfg(feature = "greptime")]
            SessionStore::Greptime {
                session: greptime_session,
                ..
            } => {
                let greptime = greptime_session.clone();
                let session_id = session.to_owned();
                let label = label.to_owned();
                let subagent = subagent_session_id.map(str::to_owned);
                match tokio::runtime::Handle::try_current() {
                    Ok(handle) => {
                        handle.spawn(async move {
                            if let Err(error) = greptime
                                .lock()
                                .await
                                .record_task_start(&session_id, id, &label, subagent.as_deref())
                                .await
                            {
                                eprintln!("e-agent: cannot record background task: {error:#}");
                            }
                        });
                    }
                    Err(_) => {
                        eprintln!("e-agent: cannot record background task: no tokio runtime");
                    }
                }
            }
        }
    }

    /// Forget one task: its completion arrived while the process was alive.
    pub fn clear_background_task(&self, root: &Path, session: &str, id: u64) {
        match self {
            SessionStore::Jsonl => Session::clear_background_task(root, session, id),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime {
                session: greptime_session,
                ..
            } => {
                let greptime = greptime_session.clone();
                let session_id = session.to_owned();
                match tokio::runtime::Handle::try_current() {
                    Ok(handle) => {
                        handle.spawn(async move {
                            if let Err(error) =
                                greptime.lock().await.clear_task(&session_id, id).await
                            {
                                eprintln!("e-agent: cannot clear background task: {error:#}");
                            }
                        });
                    }
                    Err(_) => {
                        eprintln!("e-agent: cannot clear background task: no tokio runtime");
                    }
                }
            }
        }
    }

    /// Tasks recorded by a previous process that died before their
    /// completion arrived. Consumes the record (file or table rows) and
    /// returns the labels so the caller can inject the "killed on exit"
    /// notice. Only this session's own records are returned.
    pub async fn take_unfinished_background(
        &self,
        root: &Path,
        session: &str,
    ) -> Result<Vec<String>> {
        match self {
            SessionStore::Jsonl => Ok(Session::take_unfinished_background(root, session)),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime {
                session: greptime_session,
                ..
            } => {
                let session_id = session.to_owned();
                greptime_session
                    .lock()
                    .await
                    .take_unfinished_tasks(&session_id)
                    .await
            }
        }
    }

    /// Like [`Self::take_unfinished_background`] but scoped to rows whose
    /// `subagent_session_id` matches — used when resuming a subagent
    /// session so it learns what died with its background delegates. The
    /// Greptime table is global and supports the cross-session lookup;
    /// JSONL has no per-subagent record file, so it always reports nothing.
    pub async fn take_unfinished_background_for_subagent(
        &self,
        _root: &Path,
        subagent_session_id: &str,
    ) -> Result<Vec<String>> {
        match self {
            SessionStore::Jsonl => Ok(Vec::new()),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                session
                    .lock()
                    .await
                    .take_unfinished_tasks_for_subagent(subagent_session_id)
                    .await
            }
        }
    }
}
