//! Runtime session backend dispatch. A simple enum chooses between the
//! default JSONL file backend and the optional GreptimeDB / SQLite backends
//! without introducing a trait (see AGENTS.md: "one adapter, no seam").
//!
//! Each variant holds whatever state its backend needs:
//!
//! - **Jsonl** — stateless marker; every call provides `root` + `name`.
//! - **Greptime** — a connected + session-bound client behind a Mutex so
//!   `&self` methods work everywhere (including the delegate's closure-based
//!   the runner persistence path).
//! - **Sqlite** — a connected + session-bound client behind a Mutex, same
//!   shape as Greptime; the session id is bound at connect time and the
//!   database file is shared across sessions.
//!
//! Background-task state (`*.background.jsonl` files on JSONL, the
//! `running_tasks` table on Greptime/SQLite) is dispatched through the same
//! enum: [`SessionStore::record_background_start`] /
//! [`SessionStore::clear_background_task`] /
//! [`SessionStore::take_unfinished_background`].

use std::path::{Path, PathBuf};
#[cfg(any(feature = "greptime", feature = "sqlite"))]
use std::sync::Arc;
#[cfg(any(feature = "greptime", feature = "sqlite"))]
use tokio::sync::Mutex;

use anyhow::Result;

use crate::agent::SessionEntry;
use crate::config::SessionBackend;
use crate::session::{LoadedSession, Session};

/// Sentinel session id for a workspace-scoped metadata store: only the
/// `workspace_id` bound at connect time is used by `list_meta` /
/// `backfill_sessions`; `delete_meta` takes its target explicitly, so the
/// sentinel is never matched. Only meaningful on the Greptime and SQLite
/// backends.
#[allow(dead_code)]
const META_STORE_SENTINEL: &str = "_meta";

/// One session's metadata snapshot from the `sessions` audit table
/// (Greptime and SQLite backends only). Every row is a COMPLETE snapshot —
/// Greptime has no UPDATE and SQLite follows the same append-only audit
/// shape, so the latest row per session wins by the TIME INDEX
/// (`last_active_at`); `list_meta` deduplicates per session accordingly.
/// The `workspace_id` is implied by the store/connection and never carried
/// here. The JSONL backend has no meta table and never produces these
/// values.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionMeta {
    pub session_id: String,
    pub created_at: chrono::NaiveDateTime,
    pub last_active_at: chrono::NaiveDateTime,
    pub model: Option<String>,
    pub role: Option<String>,
    pub entry_count: i64,
    pub parent_session_id: Option<String>,
    pub parent_task_id: Option<i64>,
    /// User-assigned session name (manual, never auto-generated). `None`
    /// = unnamed (the frontend shows the session id). Greptime/SQLite only —
    /// the JSONL backend has no meta table and never produces values.
    pub title: Option<String>,
    /// User pin flag: `Some(true)` = pinned (sorted first in the list),
    /// `Some(false)` = explicitly unpinned, `None` = never touched (reads
    /// as unpinned). Greptime/SQLite only — the JSONL backend has no meta
    /// table and never produces values.
    pub pinned: Option<bool>,
    /// Writer process identity of this snapshot row
    /// (`pid@hostname#nonce`, see `session_greptime::process_identity`):
    /// the process whose `insert_meta` stamped the row (the most recent
    /// insert_meta for this snapshot — the audit table records every
    /// snapshot's writer, and the latest one doubles as a best-effort hint
    /// in concurrent-write conflict errors). `None` for rows written
    /// before the column existed (the migration reads them back as NULL).
    /// Greptime/SQLite only — the JSONL backend has no meta table and never
    /// produces values.
    pub writer: Option<String>,
    /// Task-panel label of the delegate task that spawned this session
    /// (see [`SessionStore::label_for_subagent`]). Never stored in the
    /// sessions table — it lives in `running_tasks` and is resolved at
    /// list time, so this is always `None` when read from the DB.
    pub label: Option<String>,
}

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
    /// Local SQLite/turso file-backed session storage behind a mutex so
    /// `&self` methods work from any context. Each store is bound to one
    /// session (its own `next_seq` append cursor); the database file is
    /// shared across sessions of the same workspace.
    #[cfg(feature = "sqlite")]
    Sqlite {
        /// The connected SQLite session client, session-bound.
        session: Arc<Mutex<crate::session_sqlite::SqliteSession>>,
        /// Database file path, preserved so the backend config can be
        /// recovered for subagent session binding.
        path: String,
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
    /// For `Jsonl` this is a zero-cost marker; for `Greptime`/`Sqlite` it
    /// connects to the database and ensures the session table exists. The
    /// `session_id` and `workspace_id` are bound at connect time (the
    /// backend is per-session).  The `workspace_id` is derived from the
    /// canonical workspace root to namespace sessions by workspace.
    ///
    /// When a backend's cargo feature is not enabled and the config selects
    /// it, returns an error explaining the missing feature.
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
            #[cfg(feature = "sqlite")]
            SessionBackend::Sqlite { path } => {
                let workspace_id = crate::session_sqlite::derive_workspace_id(root);
                let session =
                    crate::session_sqlite::SqliteSession::connect(path, &workspace_id, session_id)
                        .await
                        .map_err(anyhow::Error::msg)?;
                Ok(SessionStore::Sqlite {
                    session: Arc::new(Mutex::new(session)),
                    path: path.clone(),
                })
            }
            #[cfg(not(feature = "sqlite"))]
            SessionBackend::Sqlite { .. } => {
                anyhow::bail!("sqlite session backend requires the `sqlite` cargo feature");
            }
        }
    }

    /// Connect a workspace-scoped store for sessions-metadata operations
    /// (`list_meta` / `backfill_sessions` / `delete_meta`). The session id
    /// is a sentinel: Greptime/SQLite operations are keyed by the
    /// `workspace_id` bound at connect time. JSONL: the registry-only
    /// marker store.
    #[allow(unused_variables)]
    pub async fn connect_meta(backend: &SessionBackend, root: &Path) -> Result<Self> {
        match backend {
            SessionBackend::Jsonl => Ok(SessionStore::Jsonl),
            #[cfg(feature = "greptime")]
            SessionBackend::Greptime { conn } => {
                let workspace_id = crate::session_greptime::derive_workspace_id(root);
                let session = crate::session_greptime::GreptimeSession::connect(
                    conn,
                    &workspace_id,
                    META_STORE_SENTINEL,
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
            #[cfg(feature = "sqlite")]
            SessionBackend::Sqlite { path } => {
                let workspace_id = crate::session_sqlite::derive_workspace_id(root);
                let session = crate::session_sqlite::SqliteSession::connect(
                    path,
                    &workspace_id,
                    META_STORE_SENTINEL,
                )
                .await
                .map_err(anyhow::Error::msg)?;
                Ok(SessionStore::Sqlite {
                    session: Arc::new(Mutex::new(session)),
                    path: path.clone(),
                })
            }
            #[cfg(not(feature = "sqlite"))]
            SessionBackend::Sqlite { .. } => {
                anyhow::bail!("sqlite session backend requires the `sqlite` cargo feature");
            }
        }
    }

    /// Return the backend configuration that was used to create this store.
    ///
    /// For JSONL returns `SessionBackend::Jsonl`; for Greptime/SQLite
    /// returns the backend variant with the stored connection string / db
    /// path.
    pub fn backend(&self) -> SessionBackend {
        match self {
            SessionStore::Jsonl => SessionBackend::Jsonl,
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { conn, .. } => SessionBackend::Greptime { conn: conn.clone() },
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { path, .. } => SessionBackend::Sqlite { path: path.clone() },
        }
    }

    /// Load session entries.
    ///
    /// For JSONL, `root` and `name` locate the file. For Greptime/SQLite,
    /// the session is already bound so `root`/`name` are unused.
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
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => {
                let entries = {
                    session
                        .lock()
                        .await
                        .load()
                        .await
                        .map_err(anyhow::Error::msg)?
                };
                Ok(LoadedSession {
                    entries,
                    legacy: false,
                })
            }
        }
    }

    /// Load only the newest compaction segment (the last `Compaction` entry
    /// and everything after it). The agent context on resume depends only
    /// on that segment, so this keeps startup cheap on long sessions;
    /// older history is pulled on demand via [`Self::load_older`].
    ///
    /// For JSONL this falls back to the full [`Self::load`] (no segmented
    /// loading on the local backend — behavior unchanged). For
    /// Greptime/SQLite it delegates to the backend's `load_head`.
    pub async fn load_head(&self, root: &Path, name: &str) -> Result<LoadedSession> {
        match self {
            SessionStore::Jsonl => Session::load(root, name),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                let entries = { session.lock().await.load_head().await? };
                Ok(LoadedSession {
                    entries,
                    legacy: false,
                })
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => {
                let entries = {
                    session
                        .lock()
                        .await
                        .load_head()
                        .await
                        .map_err(anyhow::Error::msg)?
                };
                Ok(LoadedSession {
                    entries,
                    legacy: false,
                })
            }
        }
    }

    /// Load the compaction segment immediately older than `before_seq`,
    /// returning `(entries, cursor)` where `cursor` is the seq of the
    /// compaction that opens the returned segment (`Some`) or `None` when
    /// this was the oldest segment. The caller feeds `cursor` back as the
    /// next `before_seq` to page further back; `None` means the end of
    /// history.
    ///
    /// `limit` is passed through to the Greptime/SQLite backend: when
    /// `Some(n)`, only the `n` entries of the segment closest to
    /// `before_seq` are returned (intra-segment paging, cursor = oldest seq
    /// of the page), so long sessions can be loaded in bounded pages
    /// instead of whole compaction segments.
    ///
    /// For JSONL the whole session was already loaded by [`Self::load`] /
    /// [`Self::load_head`], so there is nothing older to fetch: returns
    /// `(vec![], None)` regardless of `limit`. For Greptime/SQLite it
    /// delegates to the backend's `load_older`.
    pub async fn load_older(
        &self,
        _root: &Path,
        _name: &str,
        before_seq: i64,
        limit: Option<usize>,
    ) -> Result<(Vec<SessionEntry>, Option<i64>)> {
        match self {
            SessionStore::Jsonl => Ok((Vec::new(), None)),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                session.lock().await.load_older(before_seq, limit).await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .load_older(before_seq, limit)
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// Load the oldest compaction segment: everything before the first
    /// `Compaction` entry, returning `(entries, cursor)` where `cursor`
    /// is `Some(first_comp_seq)` when middle segments exist — feed it to
    /// [`Self::load_newer`] as the first `after_seq` — and `None` when
    /// the session has no compaction (the whole session is one head
    /// segment, already loaded by [`Self::load_head`]).
    ///
    /// For JSONL the whole session was already loaded by [`Self::load`] /
    /// [`Self::load_head`], so there is nothing older to fetch: returns
    /// `(vec![], None)`. For Greptime/SQLite it delegates to the backend's
    /// `load_oldest`.
    pub async fn load_oldest(
        &self,
        _root: &Path,
        _name: &str,
    ) -> Result<(Vec<SessionEntry>, Option<i64>)> {
        match self {
            SessionStore::Jsonl => Ok((Vec::new(), None)),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => session.lock().await.load_oldest().await,
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .load_oldest()
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// Load the compaction segment immediately newer than `after_seq`,
    /// returning `(entries, cursor)` where `cursor` is the seq of the
    /// next compaction after the returned segment (`Some`) or `None` when
    /// the head segment has been reached — the caller already holds it,
    /// so nothing new is returned. The caller feeds `cursor` back as the
    /// next `after_seq` to page further forward; `None` means the end of
    /// the middle segments.
    ///
    /// For JSONL the whole session was already loaded by [`Self::load`] /
    /// [`Self::load_head`], so there is nothing newer to fetch: returns
    /// `(vec![], None)`. For Greptime/SQLite it delegates to the backend's
    /// `load_newer`.
    pub async fn load_newer(
        &self,
        _root: &Path,
        _name: &str,
        after_seq: i64,
    ) -> Result<(Vec<SessionEntry>, Option<i64>)> {
        match self {
            SessionStore::Jsonl => Ok((Vec::new(), None)),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                session.lock().await.load_newer(after_seq).await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .load_newer(after_seq)
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// The backend seq of the newest compaction entry — the first seq of
    /// the head segment loaded by [`Self::load_head`]. `None` means the
    /// session has no compaction (the head covers the whole session, so
    /// there is nothing older to load). The TUI uses this to seed the
    /// first [`Self::load_older`] call.
    ///
    /// For JSONL there is no seq column and the full session was already
    /// loaded: always `None`.
    pub async fn head_seq(&self, _root: &Path, _name: &str) -> Result<Option<i64>> {
        match self {
            SessionStore::Jsonl => Ok(None),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => session.lock().await.head_seq().await,
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .head_seq()
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// Load entries paired with their backend sequence number, in load
    /// order. Used by `--fork` to record the source entry's seq as
    /// provenance on the `ForkedFrom` marker.
    ///
    /// For JSONL there is no seq column; the 0-based ordinal (the JSONL line
    /// index) is returned so the provenance is still meaningful. For
    /// Greptime/SQLite the real `seq` column values are returned.
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
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .load_with_seq()
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// Append entries to the session log.
    ///
    /// For JSONL, `root` and `name` locate the file. For Greptime/SQLite,
    /// the session is already bound so `root`/`name` are unused.
    pub async fn append(&self, root: &Path, name: &str, entries: &[SessionEntry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        match self {
            SessionStore::Jsonl => Session::append(root, name, entries),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => session.lock().await.append(entries).await,
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .append(entries)
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// Rewrite the entire session log (used for legacy migration).
    ///
    /// For JSONL this replaces the file atomically. For Greptime/SQLite it
    /// is a no-op — compaction is append-only.
    pub async fn rewrite(&self, root: &Path, name: &str, entries: &[SessionEntry]) -> Result<()> {
        match self {
            SessionStore::Jsonl => Session::rewrite(root, name, entries),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { .. } => Ok(()), // Append-only; rewriting is a no-op.
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .rewrite(entries)
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    // ------------------------------------------------------------------
    // Sessions metadata — the `sessions` audit table (Greptime/SQLite only)
    // ------------------------------------------------------------------
    //
    // The JSONL backend has no meta table: create/touch/delete are no-ops
    // and `list_meta` is always empty (registry-only listing, M4).

    /// Fire-and-forget activity touch on the sessions metadata table
    /// (Greptime/SQLite only): appends one full snapshot row with a fresh
    /// `last_active_at` and `entry_count = next_seq`. The write is spawned
    /// onto the current tokio runtime and never awaited — turn-boundary
    /// activity is best-effort and losing the final touch at process exit
    /// is acceptable (the audit table keeps the last committed snapshot).
    /// JSONL: no-op.
    pub fn touch_meta(&self) {
        match self {
            SessionStore::Jsonl => {}
            #[cfg(feature = "greptime")]
            SessionStore::Greptime {
                session: greptime_session,
                ..
            } => {
                let greptime = greptime_session.clone();
                match tokio::runtime::Handle::try_current() {
                    Ok(handle) => {
                        handle.spawn(async move {
                            if let Err(error) = greptime.lock().await.touch_meta().await {
                                eprintln!("e-agent: cannot touch session metadata: {error:#}");
                            }
                        });
                    }
                    Err(_) => {
                        eprintln!("e-agent: cannot touch session metadata: no tokio runtime");
                    }
                }
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite {
                session: sqlite_session,
                ..
            } => {
                let sqlite = sqlite_session.clone();
                match tokio::runtime::Handle::try_current() {
                    Ok(handle) => {
                        handle.spawn(async move {
                            if let Err(error) = sqlite.lock().await.touch_meta().await {
                                eprintln!("e-agent: cannot touch session metadata: {error:#}");
                            }
                        });
                    }
                    Err(_) => {
                        eprintln!("e-agent: cannot touch session metadata: no tokio runtime");
                    }
                }
            }
        }
    }

    /// Create the first `sessions` metadata snapshot for a session
    /// (Greptime/SQLite only). Idempotent per session: a session that
    /// already has a row is a resume, not a creation, so no second creation
    /// snapshot is appended (that would rewrite `created_at`). `model`/
    /// `role` come from the caller's configuration; `parent_session_id`/
    /// `parent_task_id` link subagent rows to their spawning delegate
    /// (main sessions pass `None`). JSONL: no-op.
    #[allow(unused_variables)]
    pub async fn create_meta(
        &self,
        _root: &Path,
        session: &str,
        model: Option<&str>,
        role: Option<&str>,
        parent_session_id: Option<&str>,
        parent_task_id: Option<i64>,
    ) -> Result<()> {
        match self {
            SessionStore::Jsonl => Ok(()),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime {
                session: greptime_session,
                ..
            } => {
                greptime_session
                    .lock()
                    .await
                    .create_meta(session, model, role, parent_session_id, parent_task_id)
                    .await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite {
                session: sqlite_session,
                ..
            } => sqlite_session
                .lock()
                .await
                .create_meta(session, model, role, parent_session_id, parent_task_id)
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// List the latest metadata snapshot per session (Greptime/SQLite
    /// only), newest activity first. JSONL: always empty (registry-only
    /// listing).
    pub async fn list_meta(&self, _root: &Path) -> Result<Vec<SessionMeta>> {
        match self {
            SessionStore::Jsonl => Ok(Vec::new()),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => session.lock().await.list_meta().await,
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .list_meta()
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// Hide a session from the sessions list by deleting ALL of its
    /// metadata rows (Greptime/SQLite only). The transcript in
    /// `session_entries` is untouched, so resume still works. JSONL: no-op.
    #[allow(unused_variables)]
    pub async fn delete_meta(&self, _root: &Path, session: &str) -> Result<()> {
        match self {
            SessionStore::Jsonl => Ok(()),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime {
                session: greptime_session,
                ..
            } => greptime_session.lock().await.delete_meta(session).await,
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite {
                session: sqlite_session,
                ..
            } => sqlite_session
                .lock()
                .await
                .delete_meta(session)
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// Rename a session in the sessions metadata table (Greptime/SQLite
    /// only): appends one full snapshot row with the new `title` and a
    /// fresh `last_active_at`. `title = None` clears the name (stored as
    /// NULL). Never self-creates (R3): a session with no metadata row is a
    /// no-op `Ok`, mirroring `touch_meta`. JSONL: no-op `Ok` — the JSONL
    /// backend has no meta table, so titles exist only on Greptime/SQLite.
    #[allow(unused_variables)]
    pub async fn set_title(&self, _root: &Path, session: &str, title: Option<&str>) -> Result<()> {
        match self {
            SessionStore::Jsonl => Ok(()),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime {
                session: greptime_session,
                ..
            } => {
                greptime_session
                    .lock()
                    .await
                    .set_title(session, title)
                    .await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite {
                session: sqlite_session,
                ..
            } => sqlite_session
                .lock()
                .await
                .set_title(session, title)
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// Pin or unpin a session in the sessions metadata table
    /// (Greptime/SQLite only): appends one full snapshot row with the new
    /// `pinned` flag and a fresh `last_active_at`. Never self-creates (R3):
    /// a session with no metadata row is a no-op `Ok`, mirroring
    /// `set_title`. JSONL: no-op `Ok` — the JSONL backend has no meta
    /// table, so pins exist only on Greptime/SQLite.
    #[allow(unused_variables)]
    pub async fn set_pinned(&self, _root: &Path, session: &str, pinned: bool) -> Result<()> {
        match self {
            SessionStore::Jsonl => Ok(()),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime {
                session: greptime_session,
                ..
            } => {
                greptime_session
                    .lock()
                    .await
                    .set_pinned(session, pinned)
                    .await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite {
                session: sqlite_session,
                ..
            } => sqlite_session
                .lock()
                .await
                .set_pinned(session, pinned)
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// One-time bootstrap migration: create metadata rows for sessions
    /// that predate the `sessions` table (they have `session_entries` but
    /// no metadata row). Idempotent: sessions that already have a row are
    /// skipped, so running it twice yields identical results. Only the
    /// server bootstrap calls this — never `connect` (L3) and never gated
    /// on table emptiness (M1). JSONL: no-op.
    pub async fn backfill_sessions(&self, _root: &Path) -> Result<()> {
        match self {
            SessionStore::Jsonl => Ok(()),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                session.lock().await.backfill_sessions().await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .backfill_sessions()
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    // ------------------------------------------------------------------
    // Background-task state records
    // ------------------------------------------------------------------
    //
    // Sync facade for the record/clear paths: JSONL writes the record file
    // synchronously; Greptime/SQLite schedule the write onto the current
    // tokio runtime (every production call site — tool completion, task
    // ack, delegate cleanup, TUI cancel — runs inside one). Errors are
    // reported on stderr: recording must never break the agent loop,
    // matching the `let _ =` callers of the old
    // `Session::record_background_start`.

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
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite {
                session: sqlite_session,
                ..
            } => {
                let sqlite = sqlite_session.clone();
                let session_id = session.to_owned();
                let label = label.to_owned();
                let subagent = subagent_session_id.map(str::to_owned);
                match tokio::runtime::Handle::try_current() {
                    Ok(handle) => {
                        handle.spawn(async move {
                            if let Err(error) = sqlite
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
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite {
                session: sqlite_session,
                ..
            } => {
                let sqlite = sqlite_session.clone();
                let session_id = session.to_owned();
                match tokio::runtime::Handle::try_current() {
                    Ok(handle) => {
                        handle.spawn(async move {
                            if let Err(error) =
                                sqlite.lock().await.clear_task(&session_id, id).await
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
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite {
                session: sqlite_session,
                ..
            } => {
                let session_id = session.to_owned();
                sqlite_session
                    .lock()
                    .await
                    .take_unfinished_tasks(&session_id)
                    .await
                    .map_err(anyhow::Error::msg)
            }
        }
    }

    /// Like [`Self::take_unfinished_background`] but scoped to rows whose
    /// `subagent_session_id` matches — used when resuming a subagent
    /// session so it learns what died with its background delegates. The
    /// Greptime/SQLite table is global and supports the cross-session
    /// lookup; JSONL has no per-subagent record file, so it always reports
    /// nothing.
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
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .take_unfinished_tasks_for_subagent(subagent_session_id)
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// The task-panel label for a subagent session: the label of the newest
    /// surviving `running_tasks` row whose `subagent_session_id` matches
    /// (rows are deleted when the delegate task completes, so `None` means
    /// "no live delegate task carries this session" and the frontend falls
    /// back to the session id). Greptime/SQLite only — the JSONL backend
    /// has no global task table and always reports `None`.
    pub async fn label_for_subagent(
        &self,
        _root: &Path,
        subagent_session_id: &str,
    ) -> Result<Option<String>> {
        match self {
            SessionStore::Jsonl => Ok(None),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                session
                    .lock()
                    .await
                    .label_for_subagent(subagent_session_id)
                    .await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .label_for_subagent(subagent_session_id)
                .await
                .map_err(anyhow::Error::msg),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The JSONL backend has no older pages (the whole session was already
    /// loaded by `load`/`load_head`): `load_older` must return empty + a
    /// `None` cursor for any `before_seq`/`limit` — the wire contract the
    /// frontend relies on (`next_before_seq: null` ⇔ nothing older).
    #[tokio::test]
    async fn jsonl_load_older_ignores_limit_and_returns_nothing() {
        let root = std::env::temp_dir();
        let store = SessionStore::Jsonl;
        let (entries, cursor) = store
            .load_older(&root, "some-session", 42, Some(200))
            .await
            .unwrap();
        assert!(entries.is_empty());
        assert_eq!(cursor, None);

        // Same without a limit, and with a cursor pointing at seq 0.
        let (entries, cursor) = store
            .load_older(&root, "some-session", 0, None)
            .await
            .unwrap();
        assert!(entries.is_empty());
        assert_eq!(cursor, None);
    }

    /// Full-stack SessionStore tests for the SQLite backend: exercise the
    /// store's `Sqlite` variant (connect → append → load round-trips, the
    /// meta ops, the sync background-task facade) against a real tempfile
    /// database file (never `:memory:`) so file persistence through the
    /// store is proven.
    #[cfg(feature = "sqlite")]
    mod sqlite {
        use super::*;

        fn temp_db() -> (tempfile::TempDir, PathBuf) {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("sessions.db");
            (dir, path)
        }

        fn backend(path: &Path) -> SessionBackend {
            SessionBackend::Sqlite {
                path: path.to_string_lossy().into_owned(),
            }
        }

        fn test_entries() -> Vec<SessionEntry> {
            use crate::agent::{AssistantMessage, Message};
            vec![
                Message::System {
                    content: "You are an agent".into(),
                }
                .into(),
                Message::User {
                    content: "hello world".into(),
                    images: vec![],
                }
                .into(),
                Message::Assistant(AssistantMessage {
                    content: Some("answer".into()),
                    tool_calls: vec![],
                    reasoning: Some("thinking step".into()),
                })
                .into(),
                SessionEntry::Notice {
                    text: "a notice".into(),
                },
            ]
        }

        /// Wait for the fire-and-forget background-record facade to land:
        /// `record_background_start` spawns onto the current runtime and is
        /// never awaited, so poll `take_unfinished_background` (which
        /// consumes) until the row appears or a deadline passes.
        async fn take_unfinished_with_retry(
            store: &SessionStore,
            root: &Path,
            session: &str,
        ) -> Vec<String> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let labels = store
                    .take_unfinished_background(root, session)
                    .await
                    .expect("take unfinished background");
                if !labels.is_empty() || std::time::Instant::now() >= deadline {
                    return labels;
                }
                tokio::task::yield_now().await;
            }
        }

        #[tokio::test]
        async fn sqlite_connect_append_load_roundtrips_through_store() {
            let (_dir, path) = temp_db();
            let root = std::env::temp_dir();
            let session = format!("test-store-sql-{}", crate::session::new_id());
            let entries = test_entries();

            // Connect per session (the store is per-session; the db file is
            // shared). First connection appends, second connection (same
            // file, same session id) reads back — proving persistence
            // through the store, not just in-process state.
            let store = SessionStore::connect(&backend(&path), &root, &session)
                .await
                .expect("connect sqlite store");
            store
                .append(&root, &session, &entries)
                .await
                .expect("append through store");

            // backend() round-trips the config used to build the store.
            assert_eq!(store.backend(), backend(&path));

            let store2 = SessionStore::connect(&backend(&path), &root, &session)
                .await
                .expect("reconnect sqlite store");
            let loaded = store2
                .load(&root, &session)
                .await
                .expect("load through store");
            assert!(!loaded.legacy);
            assert_eq!(loaded.entries.len(), entries.len());
            for (got, want) in loaded.entries.iter().zip(entries.iter()) {
                assert_eq!(got, want);
            }

            // load_head / load_with_seq go through the store too.
            let head = store2
                .load_head(&root, &session)
                .await
                .expect("load_head through store");
            assert_eq!(head.entries.len(), entries.len());
            let with_seq = store2
                .load_with_seq(&root, &session)
                .await
                .expect("load_with_seq through store");
            assert_eq!(with_seq.len(), entries.len());
            for (i, (seq, entry)) in with_seq.iter().enumerate() {
                assert_eq!(*seq, i as i64);
                assert_eq!(entry, &entries[i]);
            }

            // head_seq: no compaction → None; load_older/load_oldest/
            // load_newer: no compaction segments → nothing.
            assert_eq!(
                store2.head_seq(&root, &session).await.expect("head_seq"),
                None
            );
            let (older, cursor) = store2
                .load_older(&root, &session, 0, None)
                .await
                .expect("load_older");
            assert!(older.is_empty());
            assert_eq!(cursor, None);
            let (oldest, cursor) = store2
                .load_oldest(&root, &session)
                .await
                .expect("load_oldest");
            assert!(oldest.is_empty());
            assert_eq!(cursor, None);
            let (newer, cursor) = store2
                .load_newer(&root, &session, 0)
                .await
                .expect("load_newer");
            assert!(newer.is_empty());
            assert_eq!(cursor, None);

            // rewrite is a no-op for this backend but must not error.
            store2
                .rewrite(&root, &session, &entries)
                .await
                .expect("rewrite no-op");
        }

        #[tokio::test]
        async fn sqlite_meta_ops_through_store() {
            let (_dir, path) = temp_db();
            let root = std::env::temp_dir();
            let session_a = format!("test-store-meta-{}", crate::session::new_id());
            let session_b = format!("test-store-meta-{}", crate::session::new_id());
            let entries = test_entries();

            // Session A: transcript + metadata row.
            let store_a = SessionStore::connect(&backend(&path), &root, &session_a)
                .await
                .expect("connect sqlite store A");
            store_a
                .append(&root, &session_a, &entries)
                .await
                .expect("append A through store");
            store_a
                .create_meta(
                    &root,
                    &session_a,
                    Some("test-model"),
                    Some("main"),
                    None,
                    None,
                )
                .await
                .expect("create_meta through store");

            // Session B: transcript only — predates the meta table.
            let store_b = SessionStore::connect(&backend(&path), &root, &session_b)
                .await
                .expect("connect sqlite store B");
            store_b
                .append(&root, &session_b, &entries)
                .await
                .expect("append B through store");

            // The workspace-scoped meta store (sentinel binding) lists both
            // sessions; `session_id` at connect is irrelevant for
            // list/backfill. backfill_sessions creates a row only for B
            // (A already has one) and is idempotent.
            let meta_store = SessionStore::connect_meta(&backend(&path), &root)
                .await
                .expect("connect sqlite meta store");
            meta_store
                .backfill_sessions(&root)
                .await
                .expect("backfill_sessions through store");
            meta_store
                .backfill_sessions(&root)
                .await
                .expect("backfill_sessions idempotent");
            let list = meta_store
                .list_meta(&root)
                .await
                .expect("list_meta through store");
            assert_eq!(list.len(), 2);
            let meta_a = list
                .iter()
                .find(|meta| meta.session_id == session_a)
                .expect("session A listed");
            assert_eq!(meta_a.model.as_deref(), Some("test-model"));
            assert_eq!(meta_a.role.as_deref(), Some("main"));
            assert_eq!(meta_a.entry_count, entries.len() as i64);
            let meta_b = list
                .iter()
                .find(|meta| meta.session_id == session_b)
                .expect("session B listed");
            assert_eq!(meta_b.model, None);
            assert_eq!(meta_b.role, None);
            assert_eq!(meta_b.entry_count, entries.len() as i64);

            // set_title / set_pinned land as new snapshots.
            meta_store
                .set_title(&root, &session_a, Some("my title"))
                .await
                .expect("set_title through store");
            meta_store
                .set_pinned(&root, &session_a, true)
                .await
                .expect("set_pinned through store");
            let list = meta_store
                .list_meta(&root)
                .await
                .expect("list_meta after rename");
            let meta_a = list
                .iter()
                .find(|meta| meta.session_id == session_a)
                .expect("session A relisted");
            assert_eq!(meta_a.title.as_deref(), Some("my title"));
            assert_eq!(meta_a.pinned, Some(true));

            // delete_meta hides only session A; B stays.
            meta_store
                .delete_meta(&root, &session_a)
                .await
                .expect("delete_meta through store");
            let list = meta_store
                .list_meta(&root)
                .await
                .expect("list_meta after delete");
            assert_eq!(list.len(), 1);
            assert_eq!(list[0].session_id, session_b);

            // delete_meta never touches the transcript: A is still loadable
            // through a fresh per-session store on the same file.
            let store_a2 = SessionStore::connect(&backend(&path), &root, &session_a)
                .await
                .expect("reconnect sqlite store A");
            let loaded = store_a2
                .load(&root, &session_a)
                .await
                .expect("load A after delete");
            assert_eq!(
                loaded.entries.len(),
                entries.len(),
                "transcript survives delete_meta"
            );
        }

        #[tokio::test]
        async fn sqlite_background_task_sync_facade_through_store() {
            let (_dir, path) = temp_db();
            let root = std::env::temp_dir();
            let session = format!("test-store-bg-{}", crate::session::new_id());
            let store = SessionStore::connect(&backend(&path), &root, &session)
                .await
                .expect("connect sqlite store");

            // Sync facade: record spawns onto the current runtime.
            store.record_background_start(&root, &session, 7, "build project", None);
            let labels = take_unfinished_with_retry(&store, &root, &session).await;
            assert_eq!(
                labels,
                vec![crate::session::format_unfinished(7, "build project", None)]
            );

            // A second record + clear: clearing forgets the task, so the
            // next take reports nothing. Both record and clear are
            // fire-and-forget spawns on the current runtime; give each
            // spawn a yield window to run to completion before asserting.
            store.record_background_start(&root, &session, 8, "run tests", None);
            let landed = take_unfinished_with_retry(&store, &root, &session).await;
            assert_eq!(
                landed,
                vec![crate::session::format_unfinished(8, "run tests", None)]
            );

            // Record again, let it land, clear, let the clear land, then
            // nothing may be reported.
            store.record_background_start(&root, &session, 8, "run tests", None);
            for _ in 0..1000 {
                tokio::task::yield_now().await;
            }
            store.clear_background_task(&root, &session, 8);
            for _ in 0..1000 {
                tokio::task::yield_now().await;
            }
            let labels = store
                .take_unfinished_background(&root, &session)
                .await
                .expect("final take");
            assert!(labels.is_empty(), "cleared task must not be reported");

            // Subagent-scoped record, looked up from a DIFFERENT session's
            // store (the running_tasks table is workspace-global): the
            // non-consuming label_for_subagent reports it first, then the
            // consuming take_unfinished_background_for_subagent resolves it
            // cross-session.
            store.record_background_start(&root, &session, 9, "delegate work", Some("sub-123"));
            let sub_store = SessionStore::connect(&backend(&path), &root, "other-session")
                .await
                .expect("connect other-session store");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let label = loop {
                if let Some(label) = sub_store
                    .label_for_subagent(&root, "sub-123")
                    .await
                    .expect("label for subagent")
                {
                    break label;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "subagent record never landed"
                );
                tokio::task::yield_now().await;
            };
            assert_eq!(label, "delegate work");

            // The subagent-scoped take keys the lookup by the
            // subagent_session_id, so its labels carry no `(session: …)`
            // suffix.
            let subagent_labels = sub_store
                .take_unfinished_background_for_subagent(&root, "sub-123")
                .await
                .expect("take for subagent");
            assert_eq!(
                subagent_labels,
                vec![crate::session::format_unfinished(9, "delegate work", None)]
            );
            // Consumed: the label lookup is now empty and the by-session
            // take has nothing left for this row either.
            assert_eq!(
                sub_store
                    .label_for_subagent(&root, "sub-123")
                    .await
                    .expect("label after take"),
                None
            );
            let labels = store
                .take_unfinished_background(&root, &session)
                .await
                .expect("by-session take after subagent consume");
            assert!(labels.is_empty());
        }
    }
}
