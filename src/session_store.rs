//! Runtime session backend dispatch. A simple enum chooses between the
//! default JSONL file backend and the optional GreptimeDB backend without
//! introducing a trait (see AGENTS.md: "one adapter, no seam").
//!
//! Each variant holds whatever state its backend needs:
//!
//! - **Jsonl** — stateless marker; every call provides `root` + `name`.
//! - **Greptime** — a connected + session-bound client behind a Mutex so
//!   `&self` methods work everywhere (including the delegate's closure-based
//!   `persist_turn` which receives `&PersistConfig`).

use std::path::Path;
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

impl SessionStore {
    /// Create a new store based on the configured backend.
    ///
    /// For `Jsonl` this is a zero-cost marker; for `Greptime` it connects to
    /// the database and ensures the session table exists. The `session_id`
    /// is bound at connect time for Greptime (the backend is per-session).
    ///
    /// When the `greptime` feature is not enabled and the config selects
    /// Greptime, returns an error explaining the missing feature.
    #[allow(unused_variables)]
    pub async fn connect(backend: &SessionBackend, session_id: &str) -> Result<Self> {
        match backend {
            SessionBackend::Jsonl => Ok(SessionStore::Jsonl),
            #[cfg(feature = "greptime")]
            SessionBackend::Greptime { conn } => {
                let session =
                    crate::session_greptime::GreptimeSession::connect(conn, session_id).await?;
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
}
