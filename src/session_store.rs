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

/// Sentinel session id for a workspace-scoped metadata store: only the
/// `workspace_id` bound at connect time is used by `list_meta` /
/// `backfill_sessions`; `delete_meta` takes its target explicitly, so the
/// sentinel is never matched. Only meaningful on the Greptime backend.
#[allow(dead_code)]
const META_STORE_SENTINEL: &str = "_meta";

/// One session's metadata snapshot from the `sessions` audit table
/// (Greptime backend only). Every row is a COMPLETE snapshot — Greptime
/// has no UPDATE, so the table is append-only and the latest row per
/// session wins by the TIME INDEX (`last_active_at`); `list_meta`
/// deduplicates per session accordingly. The `workspace_id` is implied by
/// the store/connection and never carried here. The JSONL backend has no
/// meta table and never produces these values.
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

    /// Connect a workspace-scoped store for sessions-metadata operations
    /// (`list_meta` / `backfill_sessions` / `delete_meta`). The session id
    /// is a sentinel: Greptime operations are keyed by the `workspace_id`
    /// bound at connect time. JSONL: the registry-only marker store.
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

    /// Load only the newest compaction segment (the last `Compaction` entry
    /// and everything after it). The agent context on resume depends only
    /// on that segment, so this keeps startup cheap on long sessions;
    /// older history is pulled on demand via [`Self::load_older`].
    ///
    /// For JSONL this falls back to the full [`Self::load`] (no segmented
    /// loading on the local backend — behavior unchanged). For Greptime it
    /// delegates to `GreptimeSession::load_head`.
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
        }
    }

    /// Load the compaction segment immediately older than `before_seq`,
    /// returning `(entries, cursor)` where `cursor` is the seq of the
    /// compaction that opens the returned segment (`Some`) or `None` when
    /// this was the oldest segment. The caller feeds `cursor` back as the
    /// next `before_seq` to page further back; `None` means the end of
    /// history.
    ///
    /// `limit` is passed through to the Greptime backend: when `Some(n)`,
    /// only the `n` entries of the segment closest to `before_seq` are
    /// returned (intra-segment paging, cursor = oldest seq of the page),
    /// so long sessions can be loaded in bounded pages instead of whole
    /// compaction segments.
    ///
    /// For JSONL the whole session was already loaded by [`Self::load`] /
    /// [`Self::load_head`], so there is nothing older to fetch: returns
    /// `(vec![], None)` regardless of `limit`. For Greptime it delegates to
    /// `GreptimeSession::load_older`.
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
    /// `(vec![], None)`. For Greptime it delegates to
    /// `GreptimeSession::load_oldest`.
    pub async fn load_oldest(
        &self,
        _root: &Path,
        _name: &str,
    ) -> Result<(Vec<SessionEntry>, Option<i64>)> {
        match self {
            SessionStore::Jsonl => Ok((Vec::new(), None)),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => session.lock().await.load_oldest().await,
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
    /// `(vec![], None)`. For Greptime it delegates to
    /// `GreptimeSession::load_newer`.
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
    // Sessions metadata — the `sessions` audit table (Greptime only)
    // ------------------------------------------------------------------
    //
    // The JSONL backend has no meta table: create/touch/delete are no-ops
    // and `list_meta` is always empty (registry-only listing, M4).

    /// Fire-and-forget activity touch on the sessions metadata table
    /// (Greptime only): appends one full snapshot row with a fresh
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
        }
    }

    /// Create the first `sessions` metadata snapshot for a session
    /// (Greptime only). Idempotent per session: a session that already has
    /// a row is a resume, not a creation, so no second creation snapshot
    /// is appended (that would rewrite `created_at`). `model`/`role` come
    /// from the caller's configuration; `parent_session_id`/
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
        }
    }

    /// List the latest metadata snapshot per session (Greptime only),
    /// newest activity first. JSONL: always empty (registry-only listing).
    pub async fn list_meta(&self, _root: &Path) -> Result<Vec<SessionMeta>> {
        match self {
            SessionStore::Jsonl => Ok(Vec::new()),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => session.lock().await.list_meta().await,
        }
    }

    /// Hide a session from the sessions list by deleting ALL of its
    /// metadata rows (Greptime only). The transcript in `session_entries`
    /// is untouched, so resume still works. JSONL: no-op.
    #[allow(unused_variables)]
    pub async fn delete_meta(&self, _root: &Path, session: &str) -> Result<()> {
        match self {
            SessionStore::Jsonl => Ok(()),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime {
                session: greptime_session,
                ..
            } => greptime_session.lock().await.delete_meta(session).await,
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
}
