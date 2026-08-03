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

use anyhow::{Context, Result};

use crate::agent::SessionEntry;
use std::io::{BufRead, Write};

use crate::config::SessionBackend;
use crate::session::{LoadedSession, Session};

/// Sentinel session id for a workspace-scoped metadata store: only the
/// `workspace_id` bound at connect time is used by `list_meta` /
/// `backfill_sessions`; `delete_meta` takes its target explicitly, so the
/// sentinel is never matched. Only meaningful on the Greptime and SQLite
/// backends.
#[allow(dead_code)]
const META_STORE_SENTINEL: &str = "_meta";

/// Process identity for the `sessions.writer` audit column, formatted as
/// `pid@hostname#nonce`. Computed once per process (module-level
/// `OnceLock`); every metadata snapshot row is stamped with it at write
/// time so the sessions audit table (and the JSONL `.meta.jsonl` sidecar)
/// records which process wrote each row, and concurrent-write conflict
/// errors can name the likely adversary.
///
/// Why not just `pid`? A pid alone is ambiguous: the OS reuses pids across
/// restarts, so two snapshots written by *different* processes could carry
/// the same pid. Why not rely on `hostname`? `HOSTNAME` is not guaranteed
/// to be set (hence the `COMPUTERNAME` fallback for Windows, then
/// `"unknown"`). The `nonce` disambiguates pid reuse: a simple hash of the
/// boot-time `SystemTime` nanos XORed with the pid — no new dependency,
/// and different processes (or restarts with a reused pid) get different
/// values with overwhelming probability.
static PROCESS_IDENTITY: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The current process's identity string (see [`PROCESS_IDENTITY`]).
pub(crate) fn process_identity() -> &'static str {
    PROCESS_IDENTITY.get_or_init(|| {
        let pid = std::process::id();
        let hostname = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| "unknown".to_owned());
        let nonce = {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            // Simple mixing of the timestamp and pid; hex keeps the string
            // short. Same pid at a different boot time → different nonce.
            format!("{:x}", (nanos as u64) ^ (pid as u64))
        };
        format!("{pid}@{hostname}#{nonce}")
    })
}

/// Upper bound for [`SessionStore::load_head_page`]: the head segment is
/// `[last_comp, ∞)` — the last `Compaction` entry and everything after
/// it — and `i64::MAX` as the `before_seq` upper bound reuses
/// [`SessionStore::load_older`]'s intra-segment paging verbatim. With
/// `limit = Some(n)` it returns the newest `n` entries of the head with
/// the cursor at the oldest seq of the page (the truncation point; the
/// frontend feeds it back as `before_seq` to page into the cut-off part
/// of the head seamlessly). With `limit = None` it returns the whole head
/// segment with the cursor at the seq of the compaction that opens it
/// (matching the old `load_head` + `head_seq` pair). Only meaningful on
/// the Greptime and SQLite backends (JSONL has no seq-based paging).
#[allow(dead_code)]
const HEAD_OPEN_SENTINEL: i64 = i64::MAX;

/// One session's metadata snapshot from the `sessions` audit table
/// (Greptime and SQLite backends only). Every row is a COMPLETE snapshot —
/// Greptime has no UPDATE and SQLite follows the same append-only audit
/// shape, so the latest row per session wins by the TIME INDEX
/// (`last_active_at`); `list_meta` deduplicates per session accordingly.
/// The `workspace_id` is implied by the store/connection and never carried
/// here. The JSONL backend stores the same snapshots in a per-session
/// sidecar file (`.meta.jsonl`), one line per snapshot.
///
/// `Serialize`/`Deserialize` exist for the JSONL backend's per-session
/// sidecar (`.meta.jsonl`): one complete snapshot per line, mirroring one
/// audit-table row. The field layout is exactly the table columns, so a
/// sidecar snapshot and a DB row are interchangeable at `list_meta` time.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
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
    /// = unnamed (the frontend shows the session id).
    pub title: Option<String>,
    /// User pin flag: `Some(true)` = pinned (sorted first in the list),
    /// `Some(false)` = explicitly unpinned, `None` = never touched (reads
    /// as unpinned).
    pub pinned: Option<bool>,
    /// User archive flag: `Some(true)` = archived (hidden from the default
    /// session list, folded into the sidebar's collapsed "归档" group),
    /// `Some(false)` = explicitly restored, `None` = never touched (reads
    /// as unarchived).
    pub archived: Option<bool>,
    /// Writer process identity of this snapshot row
    /// (`pid@hostname#nonce`, see [`process_identity`]): the process whose
    /// insert appended the row (the audit table / sidecar records every
    /// snapshot's writer, and the latest one doubles as a best-effort hint
    /// in concurrent-write conflict errors). `None` for rows written
    /// before the column existed (the migration reads them back as NULL).
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
    /// `workspace_id` bound at connect time. JSONL: stateless — every
    /// operation takes `root` explicitly.
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

    /// Load the head segment as a bounded history page, returning
    /// `(entries, cursor)` directly (no [`LoadedSession`] wrapper).
    ///
    /// The head segment = `[last_comp, ∞)`: the last `Compaction` entry
    /// and everything after it. With `limit = Some(n)` only the newest
    /// `n` entries of the head segment are returned and the cursor is the
    /// seq of the oldest entry of that page — feeding it back as
    /// [`Self::load_older`]'s `before_seq` continues paging into the part
    /// of the head segment that was cut off, so the frontend's initial
    /// render (bounded to `limit` entries) never loses the gap between the
    /// truncated head and the older segments. With `limit = None` the
    /// whole head segment is returned and the cursor is the seq of the
    /// compaction that opens it (or `None` when the session has no
    /// compaction — the whole session is one head segment), matching the
    /// old `load_head` + `head_seq` pair.
    ///
    /// For JSONL there is no seq column, so paging is positional instead:
    /// the whole file is loaded (JSONL can only be parsed in full) but a
    /// bounded `limit` returns only the newest `limit` entries plus a
    /// cursor = the 0-based position of the slice start (the oldest entry
    /// of the page; `None` when the whole session fits — position 0).
    /// The position is an ABSOLUTE index into the append-only file, so
    /// feeding it back as [`Self::load_older`]'s `before_seq` pages into
    /// `[0, before)` and later appends (which only extend the head) never
    /// shift it. `limit = None` keeps the whole session + `None`, exactly
    /// like [`Self::load_head`].
    pub async fn load_head_page(
        &self,
        root: &Path,
        name: &str,
        limit: Option<usize>,
    ) -> Result<(Vec<SessionEntry>, Option<i64>)> {
        match self {
            SessionStore::Jsonl => {
                let entries = Session::load(root, name)?.entries;
                match limit.filter(|&n| n > 0) {
                    // Head larger than the page: newest `limit` entries +
                    // positional cursor at the slice start (always > 0
                    // here, mirroring "cursor = page's oldest seq, seq>0
                    // ⇔ more history" on the seq backends).
                    Some(n) if entries.len() > n => {
                        let start = entries.len() - n;
                        Ok((entries[start..].to_vec(), Some(start as i64)))
                    }
                    // Whole session fits (or no limit): full + None.
                    _ => Ok((entries, None)),
                }
            }
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                // The head segment is `[last_comp, ∞)`; see the
                // `HEAD_OPEN_SENTINEL` doc comment for the cursor
                // contract this reuse of `load_older` implements.
                session
                    .lock()
                    .await
                    .load_older(HEAD_OPEN_SENTINEL, limit)
                    .await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .load_older(HEAD_OPEN_SENTINEL, limit)
                .await
                .map_err(anyhow::Error::msg),
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
    /// For JSONL there is no seq-based paging, so `before_seq` is treated
    /// as an ABSOLUTE position into the append-only file: the slice
    /// `[max(0, before-limit), before)` is returned with a positional
    /// cursor `= max(0, before-limit)` (`None` when that is 0 — nothing
    /// older), and `before_seq <= 0` returns empty + `None`. Because the
    /// position is absolute, appends after the head page only extend the
    /// newer end and never shift `[0, before)`, so an in-flight paging
    /// chain stays gap-free. With `limit = None` the whole `[0, before)`
    /// remainder is returned with a `None` cursor (the JSONL analogue of
    /// the whole-segment behavior on Greptime/SQLite). For Greptime/SQLite
    /// it delegates to the backend's `load_older`.
    pub async fn load_older(
        &self,
        root: &Path,
        name: &str,
        before_seq: i64,
        limit: Option<usize>,
    ) -> Result<(Vec<SessionEntry>, Option<i64>)> {
        match self {
            SessionStore::Jsonl => {
                if before_seq <= 0 {
                    return Ok((Vec::new(), None));
                }
                let entries = Session::load(root, name)?.entries;
                let before = (before_seq as usize).min(entries.len());
                match limit.filter(|&n| n > 0) {
                    Some(n) => {
                        let start = before.saturating_sub(n);
                        let cursor = (start > 0).then_some(start as i64);
                        Ok((entries[start..before].to_vec(), cursor))
                    }
                    None => Ok((entries[..before].to_vec(), None)),
                }
            }
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
        _after_seq: i64,
    ) -> Result<(Vec<SessionEntry>, Option<i64>)> {
        match self {
            SessionStore::Jsonl => Ok((Vec::new(), None)),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                session.lock().await.load_newer(_after_seq).await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .load_newer(_after_seq)
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
    // Sessions metadata — the `sessions` audit table (Greptime/SQLite)
    // and the `.meta.jsonl` per-session sidecar (JSONL)
    // ------------------------------------------------------------------
    //
    // JSONL mirror: one complete snapshot per line in
    // `<root>/.e-agent/sessions/<id>.meta.jsonl` (aligned with the
    // `<id>.background.jsonl` record-file precedent), appended by every
    // create/touch/rename/pin/archive exactly like a DB audit row. The
    // file tail is the latest snapshot. No flock: appends are
    // last-writer-wins (unlike the DB's PK-conflict detection, a
    // deliberate trade-off of the file format), and a delete racing an
    // append simply rebuilds the file from the next snapshot.

    /// Fire-and-forget activity touch on the sessions metadata
    /// (Greptime/SQLite: appends one full snapshot row with a fresh
    /// `last_active_at` and `entry_count = next_seq`; JSONL: appends one
    /// sidecar snapshot line with `last_active_at = now` and
    /// `entry_count` = the current transcript line count). Greptime/
    /// SQLite spawn the write onto the current tokio runtime and never
    /// await it — turn-boundary activity is best-effort and losing the
    /// final touch at process exit is acceptable (the audit table keeps
    /// the last committed snapshot). JSONL writes synchronously (the file
    /// I/O is small; matching the sync facade of
    /// `record_background_start`). Never self-creates on any backend
    /// (R3): a session with no metadata row / sidecar is a no-op.
    pub fn touch_meta(&self, root: &Path, session: &str) {
        match self {
            SessionStore::Jsonl => {
                if let Err(error) = jsonl_touch_meta(root, session) {
                    eprintln!("e-agent: cannot touch session metadata: {error:#}");
                }
            }
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
    /// (Greptime/SQLite: first audit row; JSONL: first sidecar line).
    /// Idempotent per session: a session that already has a row is a
    /// resume, not a creation, so no second creation snapshot is appended
    /// (that would rewrite `created_at`). `model`/`role` come from the
    /// caller's configuration; `parent_session_id`/`parent_task_id` link
    /// subagent rows to their spawning delegate (main sessions pass
    /// `None`).
    #[allow(unused_variables)]
    pub async fn create_meta(
        &self,
        root: &Path,
        session: &str,
        model: Option<&str>,
        role: Option<&str>,
        parent_session_id: Option<&str>,
        parent_task_id: Option<i64>,
    ) -> Result<()> {
        match self {
            SessionStore::Jsonl => jsonl_create_meta(
                root,
                session,
                model,
                role,
                parent_session_id,
                parent_task_id,
            ),
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

    /// List the latest metadata snapshot per session (Greptime/SQLite:
    /// newest activity first from the audit table; JSONL: one tail-read of
    /// each transcript's sidecar), newest activity first.
    pub async fn list_meta(&self, root: &Path) -> Result<Vec<SessionMeta>> {
        match self {
            SessionStore::Jsonl => jsonl_list_meta(root),
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
    /// metadata rows (Greptime/SQLite) or the whole sidecar file (JSONL).
    /// The transcript is untouched, so resume still works.
    #[allow(unused_variables)]
    pub async fn delete_meta(&self, root: &Path, session: &str) -> Result<()> {
        match self {
            SessionStore::Jsonl => jsonl_delete_meta(root, session),
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

    /// Rename a session in the sessions metadata table (Greptime/SQLite)
    /// or sidecar (JSONL): appends one full snapshot row with the new
    /// `title` and a fresh `last_active_at`. `title = None` clears the
    /// name (stored as NULL). Never self-creates (R3): a session with no
    /// metadata row is a no-op `Ok`, mirroring `touch_meta`.
    #[allow(unused_variables)]
    pub async fn set_title(&self, root: &Path, session: &str, title: Option<&str>) -> Result<()> {
        match self {
            SessionStore::Jsonl => {
                jsonl_update_meta(root, session, |meta| meta.title = title.map(str::to_owned))
            }
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
    /// (Greptime/SQLite) or sidecar (JSONL): appends one full snapshot
    /// row with the new `pinned` flag and a fresh `last_active_at`. Never
    /// self-creates (R3): a session with no metadata row is a no-op `Ok`,
    /// mirroring `set_title`.
    #[allow(unused_variables)]
    pub async fn set_pinned(&self, root: &Path, session: &str, pinned: bool) -> Result<()> {
        match self {
            SessionStore::Jsonl => {
                jsonl_update_meta(root, session, |meta| meta.pinned = Some(pinned))
            }
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

    /// Archive or restore a session in the sessions metadata table
    /// (Greptime/SQLite) or sidecar (JSONL): appends one full snapshot row
    /// with the new `archived` flag and a fresh `last_active_at`. Never
    /// self-creates (R3): a session with no metadata row is a no-op `Ok`,
    /// mirroring `set_pinned`.
    #[allow(unused_variables)]
    pub async fn set_archived(&self, root: &Path, session: &str, archived: bool) -> Result<()> {
        match self {
            SessionStore::Jsonl => {
                jsonl_update_meta(root, session, |meta| meta.archived = Some(archived))
            }
            #[cfg(feature = "greptime")]
            SessionStore::Greptime {
                session: greptime_session,
                ..
            } => {
                greptime_session
                    .lock()
                    .await
                    .set_archived(session, archived)
                    .await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite {
                session: sqlite_session,
                ..
            } => sqlite_session
                .lock()
                .await
                .set_archived(session, archived)
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// One-time bootstrap migration: create metadata rows for sessions
    /// that predate the `sessions` table (they have `session_entries` but
    /// no metadata row). Idempotent: sessions that already have a row are
    /// skipped, so running it twice yields identical results. Only the
    /// server bootstrap calls this — never `connect` (L3) and never gated
    /// on table emptiness (M1). JSONL: writes first-line snapshots for
    /// transcripts that have no `.meta.jsonl` sidecar.
    pub async fn backfill_sessions(&self, root: &Path) -> Result<()> {
        match self {
            SessionStore::Jsonl => jsonl_backfill_sessions(root),
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
        _subagent_session_id: &str,
    ) -> Result<Vec<String>> {
        match self {
            SessionStore::Jsonl => Ok(Vec::new()),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                session
                    .lock()
                    .await
                    .take_unfinished_tasks_for_subagent(_subagent_session_id)
                    .await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .take_unfinished_tasks_for_subagent(_subagent_session_id)
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// The task-panel label for a subagent session: the label of the newest
    /// surviving `running_tasks` row whose `subagent_session_id` matches
    /// (rows are deleted when the delegate task completes, so `None` means
    /// "no live delegate task carries this session" and the frontend falls
    /// back to the session id). JSONL: scans every `<id>.background.jsonl`
    /// record file for a surviving delegate line whose `session_id`
    /// matches — record lines are removed on task completion
    /// (`clear_background_task`), so a surviving line means the delegate is
    /// still live, mirroring `running_tasks`.
    pub async fn label_for_subagent(
        &self,
        root: &Path,
        _subagent_session_id: &str,
    ) -> Result<Option<String>> {
        match self {
            SessionStore::Jsonl => Ok(jsonl_label_for_subagent(root, _subagent_session_id)),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                session
                    .lock()
                    .await
                    .label_for_subagent(_subagent_session_id)
                    .await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .label_for_subagent(_subagent_session_id)
                .await
                .map_err(anyhow::Error::msg),
        }
    }
}

// ----------------------------------------------------------------------
// JSONL metadata sidecar helpers — `.meta.jsonl`
// ----------------------------------------------------------------------

/// Path of one session's metadata sidecar: `<root>/.e-agent/sessions/
/// <id>.meta.jsonl`, mirroring the `<id>.background.jsonl` record files
/// and the `<id>.jsonl` transcripts. The name validation matches
/// `session::session_path` / `background_record_path`, so an id can never
/// escape the sessions directory.
fn jsonl_meta_sidecar_path(root: &Path, session: &str) -> anyhow::Result<PathBuf> {
    if session.is_empty()
        || !session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        anyhow::bail!("invalid session name for metadata sidecar: {session:?}");
    }
    Ok(root
        .join(".e-agent/sessions")
        .join(format!("{session}.meta.jsonl")))
}

/// Append one complete snapshot line to a session's metadata sidecar:
/// O_APPEND + `sync_all`, 0600 on creation — the append discipline of
/// `Session::append`, applied to metadata snapshots. Each line mirrors one
/// `sessions` audit-table row.
///
/// The `writer` column is stamped HERE at append time, not at construction
/// (matching the DB backends' `insert_meta`): every snapshot line records
/// the process that actually wrote it, so a touch/set/backfill that
/// carries a snapshot forward never replays a stale identity from an
/// earlier line. Callers construct with `writer: None`; the stamped value
/// is what lands in the file.
fn jsonl_append_meta_snapshot(
    root: &Path,
    session: &str,
    meta: &mut SessionMeta,
) -> anyhow::Result<()> {
    meta.writer = Some(process_identity().to_owned());
    let path = jsonl_meta_sidecar_path(root, session)?;
    let directory = path.parent().expect("sidecar path always has a parent");
    std::fs::create_dir_all(directory)
        .with_context(|| format!("cannot create session directory {}", directory.display()))?;
    #[cfg(unix)]
    let created = !path.exists();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("cannot append session metadata {}", path.display()))?;
    file.write_all(&serde_json::to_vec(meta)?)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    #[cfg(unix)]
    if created {
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    }
    Ok(())
}

/// The newest snapshot of a session's metadata sidecar, or `None` when the
/// sidecar does not exist or holds no parseable line. Corrupt lines are
/// skipped: the file is append-only and only the tail matters (a corrupt
/// tail is treated as "no newer snapshot").
fn jsonl_read_meta_snapshot(root: &Path, session: &str) -> anyhow::Result<Option<SessionMeta>> {
    let path = jsonl_meta_sidecar_path(root, session)?;
    let Ok(file) = std::fs::File::open(&path) else {
        return Ok(None);
    };
    let mut latest: Option<SessionMeta> = None;
    for line in std::io::BufReader::new(file)
        .lines()
        .map_while(|line| line.ok())
    {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(meta) = serde_json::from_str::<SessionMeta>(&line) {
            latest = Some(meta);
        }
    }
    Ok(latest)
}

/// Count a session's transcript entries by counting the non-empty lines of
/// `<id>.jsonl` (one serde-JSON entry per line, so no embedded raw
/// newlines). Missing transcript = 0. This is the JSONL equivalent of the
/// DB backends' `next_seq`: seqs are dense per append, so the line count
/// equals `max(seq)+1` — never a physical overcount.
fn jsonl_count_transcript_entries(root: &Path, session: &str) -> anyhow::Result<i64> {
    let sidecar = jsonl_meta_sidecar_path(root, session)?; // validates the name
    let transcript = sidecar.with_file_name(format!("{session}.jsonl"));
    let Ok(file) = std::fs::File::open(&transcript) else {
        return Ok(0);
    };
    let mut count: i64 = 0;
    for line in std::io::BufReader::new(file)
        .lines()
        .map_while(|line| line.ok())
    {
        if !line.trim().is_empty() {
            count += 1;
        }
    }
    Ok(count)
}

/// `create_meta` for the JSONL backend: write the first snapshot line, or
/// backfill what is missing when a sidecar already exists. Mirrors the DB
/// backends exactly:
///
/// - No sidecar → write the creation snapshot (`entry_count` = current
///   transcript line count, 0 for a brand-new session whose transcript
///   does not exist yet).
/// - Sidecar exists and the caller supplies a `model` or
///   `parent_session_id` the existing snapshot lacks (a backfill-created
///   first line has `model = NULL`; a subagent row written by the parent
///   at spawn time may lack the link the builder later learns) → append
///   ONE fresh snapshot carrying the missing fields in, preserving every
///   existing field and `created_at`, with a fresh `last_active_at` —
///   the DB's `backfill_meta_snapshot` semantics. A second call finds the
///   field already set and appends nothing (backfill once).
/// - Sidecar exists and nothing is missing → no-op (an idempotent
///   resume — a second creation line would rewrite `created_at` and
///   pollute the audit file).
///
/// `entry_count` mirrors the DB backends' `next_seq` at creation: the
/// transcript's current line count.
fn jsonl_create_meta(
    root: &Path,
    session: &str,
    model: Option<&str>,
    role: Option<&str>,
    parent_session_id: Option<&str>,
    parent_task_id: Option<i64>,
) -> anyhow::Result<()> {
    if let Some(existing) = jsonl_read_meta_snapshot(root, session)? {
        // Row already exists (resume, or a backfill/btw/subagent row whose
        // model or parent link was unknown at write time): only ever
        // backfill what is MISSING — never overwrite an existing model or
        // link, and never append without something to record (R3-adjacent:
        // exactly the DB's `backfill_parent || backfill_model` gate).
        let backfill_parent = parent_session_id.is_some() && existing.parent_session_id.is_none();
        let backfill_model = model.is_some() && existing.model.is_none();
        if backfill_parent || backfill_model {
            let mut meta = SessionMeta {
                session_id: existing.session_id.clone(),
                created_at: existing.created_at,
                last_active_at: chrono::Utc::now().naive_utc(),
                // Caller-supplied fields win over the existing values; the
                // rest is carried over untouched (DB backfill_meta_snapshot).
                model: model.map(str::to_owned).or_else(|| existing.model.clone()),
                role: role.map(str::to_owned).or_else(|| existing.role.clone()),
                entry_count: existing.entry_count,
                parent_session_id: parent_session_id
                    .map(str::to_owned)
                    .or_else(|| existing.parent_session_id.clone()),
                parent_task_id: parent_task_id.or(existing.parent_task_id),
                title: existing.title.clone(),
                pinned: existing.pinned,
                archived: existing.archived,
                writer: None, // stamped by jsonl_append_meta_snapshot with the writing process
                label: None, // label lives in running_tasks / background records, resolved at list time
            };
            return jsonl_append_meta_snapshot(root, session, &mut meta);
        }
        return Ok(()); // resume: nothing missing, never rewrite created_at
    }
    let now = chrono::Utc::now().naive_utc();
    let mut meta = SessionMeta {
        session_id: session.to_owned(),
        created_at: now,
        last_active_at: now,
        model: model.map(str::to_owned),
        role: role.map(str::to_owned),
        entry_count: jsonl_count_transcript_entries(root, session)?,
        parent_session_id: parent_session_id.map(str::to_owned),
        parent_task_id,
        title: None,    // a fresh session is unnamed until the user names it
        pinned: None,   // a fresh session is unpinned until the user pins it
        archived: None, // a fresh session is unarchived until the user archives it
        writer: None,   // stamped by jsonl_append_meta_snapshot with the writing process
        label: None,    // label lives in running_tasks / background records, resolved at list time
    };
    jsonl_append_meta_snapshot(root, session, &mut meta)
}

/// Shared read-tail → mutate → append for the flag setters (title, pin,
/// archive). Never self-creates (R3): a missing sidecar is a no-op `Ok`,
/// mirroring the DB backends. A fresh `last_active_at` is stamped here and
/// `entry_count` is carried from the tail snapshot (the DB backends bump
/// only `last_active_at` on these writes — the touch additionally
/// refreshes `entry_count`, which is why it has its own function).
fn jsonl_update_meta(
    root: &Path,
    session: &str,
    mutate: impl FnOnce(&mut SessionMeta),
) -> anyhow::Result<()> {
    let Some(mut meta) = jsonl_read_meta_snapshot(root, session)? else {
        return Ok(()); // R3: never self-create
    };
    mutate(&mut meta);
    meta.last_active_at = chrono::Utc::now().naive_utc();
    jsonl_append_meta_snapshot(root, session, &mut meta)
}

/// `touch_meta` for the JSONL backend: append one fresh snapshot line with
/// `last_active_at = now` and `entry_count` = the current transcript line
/// count, or no-op when no sidecar exists (R3 — a sidecar-less session
/// must not fabricate its own row). The transcript is re-counted on every
/// call: the store is stateless, and the count is the exact JSONL analogue
/// of the DB's `next_seq`. Called synchronously at turn boundaries (like
/// `record_background_start`); the read is a few hundred KB at worst.
fn jsonl_touch_meta(root: &Path, session: &str) -> anyhow::Result<()> {
    let Some(mut meta) = jsonl_read_meta_snapshot(root, session)? else {
        return Ok(()); // R3: never self-create
    };
    meta.last_active_at = chrono::Utc::now().naive_utc();
    meta.entry_count = jsonl_count_transcript_entries(root, session)?;
    jsonl_append_meta_snapshot(root, session, &mut meta)
}

/// `list_meta` for the JSONL backend: scan `.e-agent/sessions/` for every
/// metadata sidecar (`<id>.meta.jsonl` — the sessions-table mirror, so a
/// freshly created zero-turn session is listed exactly like a DB row whose
/// `session_entries` are still empty) and tail-read the newest snapshot of
/// each. Transcripts without a sidecar are skipped: listing stays
/// read-only, and the server bootstrap's `backfill_sessions` is the
/// dedicated migration that gives them first lines (mirroring the DB
/// backends, whose `list_meta` never self-creates rows either).
/// `.background.jsonl` record files and transcripts are not sidecars, so
/// they can never be mistaken for sessions. Sorted
/// newest-activity-first, matching the SQLite query's ORDER BY.
fn jsonl_list_meta(root: &Path) -> anyhow::Result<Vec<SessionMeta>> {
    let directory = root.join(".e-agent/sessions");
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return Ok(Vec::new()); // no sessions directory yet
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Session ids cannot contain `.`, so `<id>.meta.jsonl` is
        // unambiguous: the only files with this suffix are sidecars.
        let Some(session_id) = name.strip_suffix(".meta.jsonl") else {
            continue;
        };
        if let Some(meta) = jsonl_read_meta_snapshot(root, session_id)? {
            out.push(meta);
        }
    }
    out.sort_by(|a, b| b.last_active_at.cmp(&a.last_active_at));
    Ok(out)
}

/// `delete_meta` for the JSONL backend: remove the session's sidecar file.
/// The transcript stays, so resume still works. A missing sidecar is `Ok`
/// (idempotent, matching the DB's zero-row DELETE). Known limitation
/// (mirroring the audit-log design without tombstones): the next
/// `backfill_sessions` bootstrap run re-creates the file from the
/// transcript, so hiding is scoped to the current server lifetime.
fn jsonl_delete_meta(root: &Path, session: &str) -> anyhow::Result<()> {
    let path = jsonl_meta_sidecar_path(root, session)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// `backfill_sessions` for the JSONL backend: for every transcript without
/// a `.meta.jsonl` sidecar, write a first snapshot line so historical
/// sessions become visible in the list (the JSONL analogue of the DB
/// backfill that aggregates `session_entries`). Idempotent: sessions that
/// already have a sidecar are untouched, so running it twice yields
/// identical results. Called once by the server bootstrap; never from
/// `connect` (L3).
///
/// The transcript format carries no entry timestamps (unlike the DB's
/// `event_time_us`), so both `created_at` and `last_active_at` are taken
/// from the transcript file's mtime — the only recoverable activity signal
/// for a pre-sidecar session. `entry_count` is the transcript line count.
/// Legacy `.json` transcripts are excluded (they are rewritten to `.jsonl`
/// on their next resume).
fn jsonl_backfill_sessions(root: &Path) -> anyhow::Result<()> {
    let directory = root.join(".e-agent/sessions");
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return Ok(()); // no sessions directory yet
    };
    let mut sessions: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let id = name.strip_suffix(".jsonl")?.to_owned();
            if id.ends_with(".meta") || id.ends_with(".background") {
                return None; // sidecar / background record, not a transcript
            }
            Some(id)
        })
        .collect();
    sessions.sort();
    for session in sessions {
        if jsonl_read_meta_snapshot(root, &session)?.is_some() {
            continue; // already has a snapshot (idempotent)
        }
        let mtime = std::fs::metadata(directory.join(format!("{session}.jsonl")))
            .and_then(|meta| meta.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let at = chrono::DateTime::<chrono::Utc>::from(mtime).naive_utc();
        let mut meta = SessionMeta {
            session_id: session.clone(),
            created_at: at,
            last_active_at: at,
            model: None,
            role: None,
            entry_count: jsonl_count_transcript_entries(root, &session)?,
            parent_session_id: None,
            parent_task_id: None,
            title: None,    // pre-sidecar sessions have no user-assigned name
            pinned: None,   // pre-sidecar sessions are unpinned
            archived: None, // pre-sidecar sessions are unarchived
            writer: None,   // stamped by jsonl_append_meta_snapshot with the writing process
            label: None, // label lives in running_tasks / background records, resolved at list time
        };
        jsonl_append_meta_snapshot(root, &session, &mut meta)?;
    }
    Ok(())
}

/// `label_for_subagent` for the JSONL backend: scan every surviving
/// `<id>.background.jsonl` record file for a delegate line whose
/// `session_id` matches the subagent, and return the last (newest)
/// matching label. Record lines are removed when the delegate task
/// completes (`clear_background_task`), so a surviving line means the
/// delegate is still live — exactly the `running_tasks` contract. Files
/// are scanned in sorted name order and lines in append order, so "newest"
/// is deterministic; a subagent with several live delegates reports the
/// most recently recorded one. Non-destructive (called from the sessions
/// list, like the DB lookup).
fn jsonl_label_for_subagent(root: &Path, subagent_session_id: &str) -> Option<String> {
    let directory = root.join(".e-agent/sessions");
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return None;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().ends_with(".background.jsonl"))
                .unwrap_or(false)
        })
        .collect();
    files.sort();
    let mut label: Option<String> = None;
    for file in files {
        let Ok(reader) = std::fs::File::open(&file) else {
            continue;
        };
        for line in std::io::BufReader::new(reader)
            .lines()
            .map_while(|line| line.ok())
        {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(record) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if record["session_id"].as_str() == Some(subagent_session_id)
                && let Some(text) = record["label"].as_str()
            {
                label = Some(text.to_owned());
            }
        }
    }
    label
}

#[cfg(test)]
mod tests {
    use super::*;

    /// JSONL `load_older` pages by ABSOLUTE position (there is no seq
    /// column): `before_seq` is the 0-based file position, the returned
    /// slice is `[max(0, before-limit), before)`, and the cursor is the
    /// slice start (`None` at position 0 / `before_seq <= 0`).
    #[tokio::test]
    async fn jsonl_load_older_pages_by_absolute_position() {
        use crate::agent::Message;
        use crate::session::Session;

        let root = std::env::temp_dir();
        let name = format!("test-jsonl-older-{}", crate::session::new_id());
        let entries: Vec<SessionEntry> = (0..5)
            .map(|i| SessionEntry::Message {
                message: Message::User {
                    content: format!("m{i}"),
                    images: vec![],
                },
            })
            .collect();
        Session::append(&root, &name, &entries).unwrap();
        let store = SessionStore::Jsonl;

        // before_seq <= 0: nothing older.
        let (page, cursor) = store.load_older(&root, &name, 0, Some(200)).await.unwrap();
        assert!(page.is_empty());
        assert_eq!(cursor, None);

        // before=5, limit=2 → [3,5) = the two oldest-of-the-page entries,
        // cursor 3 (>0 → Some).
        let (page, cursor) = store.load_older(&root, &name, 5, Some(2)).await.unwrap();
        assert_eq!(page, vec![entries[3].clone(), entries[4].clone()]);
        assert_eq!(cursor, Some(3));

        // before=3, limit=2 → [1,3); cursor 1.
        let (page, cursor) = store.load_older(&root, &name, 3, Some(2)).await.unwrap();
        assert_eq!(page, vec![entries[1].clone(), entries[2].clone()]);
        assert_eq!(cursor, Some(1));

        // before=1, limit=2 → [0,1); the slice start is 0 → cursor None.
        let (page, cursor) = store.load_older(&root, &name, 1, Some(2)).await.unwrap();
        assert_eq!(page, vec![entries[0].clone()]);
        assert_eq!(cursor, None);

        // before > file length is clamped to the file length.
        let (page, cursor) = store.load_older(&root, &name, 42, Some(200)).await.unwrap();
        assert_eq!(page, entries, "before beyond EOF returns everything");
        assert_eq!(cursor, None);

        // limit = None: whole [0, before) remainder + None.
        let (page, cursor) = store.load_older(&root, &name, 3, None).await.unwrap();
        assert_eq!(page, entries[..3].to_vec());
        assert_eq!(cursor, None);

        // Unknown session: empty + None.
        let (page, cursor) = store
            .load_older(&root, "some-session", 42, Some(200))
            .await
            .unwrap();
        assert!(page.is_empty());
        assert_eq!(cursor, None);
    }

    /// JSONL `load_head_page` pages the head by position: when the whole
    /// session fits in the limit the full session + `None` cursor is
    /// returned (nothing is ever cut off on the local backend); when the
    /// head is larger than the limit, only the newest `limit` entries are
    /// returned with a positional cursor that pages back into the cut-off
    /// part via [`Self::load_older`].
    #[tokio::test]
    async fn jsonl_load_head_page_bounded_by_position_with_cursor() {
        use crate::agent::{AssistantMessage, Message};
        use crate::session::Session;

        let root = std::env::temp_dir();
        let name = format!("test-jsonl-head-page-{}", crate::session::new_id());
        let entries: Vec<SessionEntry> = vec![
            Message::System {
                content: "You are an agent".into(),
            }
            .into(),
            Message::User {
                content: "hello".into(),
                images: vec![],
            }
            .into(),
            Message::Assistant(AssistantMessage {
                content: Some("answer".into()),
                tool_calls: vec![],
                reasoning: None,
            })
            .into(),
        ];
        Session::append(&root, &name, &entries).unwrap();

        let store = SessionStore::Jsonl;
        // Head ≤ limit: bounded page but still the full session + None.
        let (page, cursor) = store
            .load_head_page(&root, &name, Some(3))
            .await
            .expect("load_head_page with limit >= len");
        assert_eq!(page, entries, "whole session fits → full session");
        assert_eq!(cursor, None, "position 0 → no older cursor");

        // Head > limit: newest `limit` entries + positional cursor at the
        // slice start (position 1 here — the 2 newest of a 3-entry file).
        let (page, cursor) = store
            .load_head_page(&root, &name, Some(2))
            .await
            .expect("load_head_page with limit < len");
        assert_eq!(
            page,
            entries[1..].to_vec(),
            "head > limit → newest limit entries"
        );
        assert_eq!(cursor, Some(1), "cursor = position of the slice start");

        // Without a limit: full session + None.
        let (page, cursor) = store
            .load_head_page(&root, &name, None)
            .await
            .expect("load_head_page without limit");
        assert_eq!(page, entries, "JSONL must return the full session");
        assert_eq!(cursor, None, "no limit → no cursor");
    }

    /// JSONL positional paging chain: `load_head_page` with a head larger
    /// than the limit returns the newest `limit` entries + a cursor; the
    /// cursor fed back into `load_older` completes the older part — the
    /// whole chain covers every entry exactly once (no gaps, no
    /// duplicates), matching the `load_older_head_open_sentinel_pages_...`
    /// contract of the seq backends.
    #[tokio::test]
    async fn jsonl_head_page_positional_chain_completes_without_gaps() {
        use crate::agent::Message;
        use crate::session::Session;

        let root = std::env::temp_dir();
        let name = format!("test-jsonl-chain-{}", crate::session::new_id());
        let entries: Vec<SessionEntry> = (0..7)
            .map(|i| SessionEntry::Message {
                message: Message::User {
                    content: format!("m{i}"),
                    images: vec![],
                },
            })
            .collect();
        Session::append(&root, &name, &entries).unwrap();

        let store = SessionStore::Jsonl;
        let mut paged: Vec<SessionEntry> = Vec::new();
        let mut cursor: Option<i64> = Some(i64::MAX); // head open sentinel
        loop {
            let (page, next) = match cursor {
                Some(i64::MAX) => store
                    .load_head_page(&root, &name, Some(2))
                    .await
                    .expect("head page"),
                Some(before) => store
                    .load_older(&root, &name, before, Some(2))
                    .await
                    .expect("older page"),
                None => break,
            };
            paged.extend(page);
            cursor = next;
        }
        // Exactly-once coverage (multiset, like the seq-backend chain
        // tests): same length as the file and every entry present.
        assert_eq!(
            paged.len(),
            entries.len(),
            "paged chain must not lose or duplicate entries"
        );
        for want in &entries {
            assert!(paged.contains(want), "paged chain missing entry: {want:?}");
        }

        // Spot-check the first page + the cut-off part.
        let (page, cursor) = store
            .load_head_page(&root, &name, Some(2))
            .await
            .expect("head page");
        assert_eq!(page, entries[5..].to_vec(), "newest 2 of the head");
        let (next, _) = store
            .load_older(&root, &name, cursor.unwrap(), Some(2))
            .await
            .expect("older page");
        assert_eq!(next, entries[3..5].to_vec(), "cut-off head part via cursor");
    }

    /// JSONL append stability: the positional cursor is an absolute index
    /// into the append-only file, so appending new entries after a head
    /// page must not shift `[0, before)` — the old cursor still pages the
    /// exact same older slice, and a fresh head page reflects the new head.
    #[tokio::test]
    async fn jsonl_head_page_cursor_stable_across_append() {
        use crate::agent::Message;
        use crate::session::Session;

        let root = std::env::temp_dir();
        let name = format!("test-jsonl-stable-{}", crate::session::new_id());
        let entries: Vec<SessionEntry> = (0..5)
            .map(|i| SessionEntry::Message {
                message: Message::User {
                    content: format!("m{i}"),
                    images: vec![],
                },
            })
            .collect();
        Session::append(&root, &name, &entries).unwrap();

        let store = SessionStore::Jsonl;
        // Head page over a 5-entry file with limit 3: newest 3, cursor 2.
        let (page, cursor) = store
            .load_head_page(&root, &name, Some(3))
            .await
            .expect("head page before append");
        assert_eq!(page, entries[2..].to_vec());
        assert_eq!(cursor, Some(2));

        // Append two more entries; the file now has 7 entries.
        let more: Vec<SessionEntry> = (5..7)
            .map(|i| SessionEntry::Message {
                message: Message::User {
                    content: format!("m{i}"),
                    images: vec![],
                },
            })
            .collect();
        Session::append(&root, &name, &more).unwrap();

        // The old cursor (position 2) still pages exactly [0,2) — appends
        // only extended the newer end, the absolute position did not drift.
        let (older, next) = store
            .load_older(&root, &name, cursor.unwrap(), Some(3))
            .await
            .expect("older page after append");
        assert_eq!(older, entries[..2].to_vec(), "old cursor must not drift");
        assert_eq!(next, None, "position 0 → nothing older");

        // A fresh head page sees the appended head: newest 3 = entries 4,5,6,
        // cursor = position 4 (still absolute).
        let (page, cursor) = store
            .load_head_page(&root, &name, Some(3))
            .await
            .expect("head page after append");
        assert_eq!(
            page,
            entries[4..]
                .iter()
                .cloned()
                .chain(more.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(cursor, Some(4));
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

            // set_title / set_pinned / set_archived land as new snapshots.
            meta_store
                .set_title(&root, &session_a, Some("my title"))
                .await
                .expect("set_title through store");
            meta_store
                .set_pinned(&root, &session_a, true)
                .await
                .expect("set_pinned through store");
            meta_store
                .set_archived(&root, &session_a, true)
                .await
                .expect("set_archived through store");
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
            assert_eq!(meta_a.archived, Some(true));

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

        /// `load_head_page` on the SQLite store: the bounded head page
        /// must never strand the truncated part of the head segment —
        /// the returned cursor feeds straight back into `load_older` and
        /// the whole chain covers every entry exactly once (the
        /// `GET /history` initial-render bug this fixes).
        #[tokio::test]
        async fn sqlite_load_head_page_pages_without_losing_segments() {
            use crate::agent::Message;

            let (_dir, path) = temp_db();
            let root = std::env::temp_dir();

            let user = |i: u32| SessionEntry::Message {
                message: Message::User {
                    content: format!("m{i}"),
                    images: vec![],
                },
            };
            let comp = |summary: &str| SessionEntry::Compaction {
                summary: summary.into(),
                retained: vec![],
            };

            // ---- No compaction: the whole session is one head segment.
            // With no limit the whole session comes back with a None
            // cursor (nothing older to page)...
            let session_a = format!("test-store-head-page-a-{}", crate::session::new_id());
            let store_a = SessionStore::connect(&backend(&path), &root, &session_a)
                .await
                .expect("connect sqlite store A");
            let plain = vec![user(1), user(2), user(3)];
            store_a
                .append(&root, &session_a, &plain)
                .await
                .expect("append A");
            let (page, cursor) = store_a
                .load_head_page(&root, &session_a, None)
                .await
                .expect("load_head_page A without limit");
            assert_eq!(page, plain, "no-compaction session returned whole");
            assert_eq!(cursor, None, "no compaction → no older cursor");
            // ...and a bounded page still pages through the rest of the
            // session (nothing is stranded).
            let (page, cursor) = store_a
                .load_head_page(&root, &session_a, Some(2))
                .await
                .expect("load_head_page A with limit");
            assert_eq!(page, vec![user(2), user(3)], "newest limit entries");
            assert_eq!(cursor, Some(1), "cursor = oldest seq of the page");
            let (rest, cursor) = store_a
                .load_older(&root, &session_a, cursor.unwrap(), Some(2))
                .await
                .expect("load_older A");
            assert_eq!(rest, vec![user(1)], "remaining entry reachable");
            assert_eq!(cursor, None, "seq 0 page → nothing older");

            // ---- Compaction with head ≤ limit: whole head segment +
            // cursor = the opening compaction's seq.
            let session_b = format!("test-store-head-page-b-{}", crate::session::new_id());
            let store_b = SessionStore::connect(&backend(&path), &root, &session_b)
                .await
                .expect("connect sqlite store B");
            // seqs: 0,1 early; 2 comp1; 3,4 middle; 5 comp2; 6,7 latest.
            let mut all = vec![user(1), user(2)];
            all.push(comp("c1"));
            all.extend([user(3), user(4)]);
            all.push(comp("c2"));
            all.extend([user(5), user(6)]);
            store_b
                .append(&root, &session_b, &all)
                .await
                .expect("append B");
            let (page, cursor) = store_b
                .load_head_page(&root, &session_b, Some(3))
                .await
                .expect("load_head_page B with limit");
            assert_eq!(
                page,
                vec![comp("c2"), user(5), user(6)],
                "head ≤ limit → whole head segment"
            );
            assert_eq!(cursor, Some(5), "cursor = opening compaction seq");

            // ---- Compaction with head > limit: newest `limit` entries,
            // cursor = oldest seq of the page; paging back with that
            // cursor reaches the cut-off part, then crosses compaction
            // boundaries — every entry is covered exactly once.
            let session_c = format!("test-store-head-page-c-{}", crate::session::new_id());
            let store_c = SessionStore::connect(&backend(&path), &root, &session_c)
                .await
                .expect("connect sqlite store C");
            // seqs: 0,1 early; 2 comp1; 3,4 middle; 5 comp2; 6..9 latest.
            let mut all = vec![user(1), user(2)];
            all.push(comp("c1"));
            all.extend([user(3), user(4)]);
            all.push(comp("c2"));
            all.extend([user(5), user(6), user(7), user(8)]);
            store_c
                .append(&root, &session_c, &all)
                .await
                .expect("append C");

            let mut paged: Vec<SessionEntry> = Vec::new();
            let mut cursor: Option<i64> = Some(i64::MAX); // head open sentinel
            loop {
                let (entries, next) = match cursor {
                    Some(i64::MAX) => store_c
                        .load_head_page(&root, &session_c, Some(2))
                        .await
                        .expect("head page"),
                    Some(before) => store_c
                        .load_older(&root, &session_c, before, Some(2))
                        .await
                        .expect("older page"),
                    None => break,
                };
                paged.extend(entries);
                cursor = next;
            }
            // Verify exactly-once coverage: the paged chain must contain every
            // session entry, with no gaps and no duplicates. Page boundaries
            // depend on the compaction layout (the opening compaction rides with
            // each page), so compare as multisets — the newest-first page order
            // and per-page chronological order are already spot-checked below.
            assert_eq!(
                paged.len(),
                all.len(),
                "paged chain must not lose or duplicate entries"
            );
            for want in &all {
                assert!(paged.contains(want), "paged chain missing entry: {want:?}");
            }

            // Spot-check the boundary page explicitly: the first page is
            // the newest 2 of the head, and its cursor pages into the
            // cut-off head part (not past the head into older segments).
            let (page, cursor) = store_c
                .load_head_page(&root, &session_c, Some(2))
                .await
                .expect("head page C");
            assert_eq!(page, vec![user(7), user(8)], "newest 2 of head");
            let (next, _) = store_c
                .load_older(&root, &session_c, cursor.unwrap(), Some(2))
                .await
                .expect("older page C");
            assert_eq!(
                next,
                vec![user(5), user(6)],
                "cut-off head part reachable via cursor"
            );
        }
    }
}

#[cfg(test)]
#[path = "session_jsonl_meta_tests.rs"]
mod jsonl_meta_tests;
