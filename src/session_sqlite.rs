//! SQLite/turso-backed session storage. Optional runtime-selectable
//! backend via `[session] backend = "sqlite"`. Uses the pure-Rust turso
//! engine (a SQLite-compatible in-process database, no C toolchain) for
//! the `session_entries` transcript table, the `running_tasks` state table
//! and the `sessions` metadata audit table — the same three-table layout
//! and behavioral contracts as the GreptimeDB backend (see
//! [`crate::session_greptime`]), against a local database file
//! (`:memory:` works for tests) instead of a remote database.
//!
//! Timestamps are stored as INTEGER microseconds-since-epoch
//! (`event_time_us` / `last_active_at` / `started_at_us`) and converted
//! back to `chrono::NaiveDateTime` on the read path, so callers see
//! exactly the types the Greptime backend returns.
//!
//! Non-goals: no Storage trait, no migration of existing JSONL sessions.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::agent::SessionEntry;
use crate::session_store::{
    SessionMeta, UsageRow, datetime_to_us, dedup_raw_entries, entry_kind, format_conflict_error,
    is_error, next_event_time_us, process_identity, us_to_datetime,
};
// Public path preserved for symmetry with `session_greptime` (the function
// was a `pub fn` defined here before the shared-helper extraction).
pub use crate::session_store::derive_workspace_id;

/// DDL for the session entries table. Idempotent. The primary key carries
/// `seq` + `event_time_us` so same-seq retries (a new `event_time_us`
/// every write, see [`next_event_time_us`]) append a NEW physical row
/// instead of colliding; the read path deduplicates per seq keeping the
/// latest event_time (see [`dedup_raw_entries`]) — exactly Greptime's
/// contract, with the append log modeled on a real row-level PK instead of
/// the engine's append mode.
const CREATE_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS session_entries (
    workspace_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    event_time_us INTEGER NOT NULL,
    entry_kind TEXT NOT NULL,
    payload TEXT NOT NULL,
    schema_version INTEGER NOT NULL DEFAULT 1,
    is_error INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (workspace_id, session_id, seq, event_time_us)
)
"#;

/// DDL for the background-task state table. Idempotent. One row per
/// in-flight background task (bash or delegate), scoped to
/// (workspace, session) like `session_entries`; `subagent_session_id`
/// links delegate rows back to the subagent session they spawned, so a
/// resumed subagent can find its own killed tasks by a global lookup.
///
/// Rows are consumed (DELETE) on completion (`clear_task`) or on resume
/// (`take_unfinished_tasks`), so a surviving row always means "the process
/// died with this task running". Unlike `session_entries` this is a state
/// table, not a log: real UPDATE/DELETE is allowed, and `record_task_start`
/// upserts (`INSERT ... ON CONFLICT DO UPDATE`), so re-recording a
/// `task_id` (the per-process background counter may repeat across
/// restarts) simply overwrites the old row.
const CREATE_TABLE_RUNNING_TASKS: &str = r#"
CREATE TABLE IF NOT EXISTS running_tasks (
    workspace_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    task_id INTEGER NOT NULL,
    label TEXT NOT NULL,
    subagent_session_id TEXT NULL,
    started_at_us INTEGER NOT NULL,
    owner_identity TEXT NULL,
    PRIMARY KEY (workspace_id, session_id, task_id)
)
"#;

/// DDL for the sessions metadata table. Idempotent.
///
/// Semantics: an append-only lifecycle AUDIT log, not a one-row-per-session
/// upsert table. Every create/touch appends a COMPLETE snapshot row
/// (created_at/model/role/parent/entry_count/title/pinned/archived/writer
/// all carried on every row) at a fresh `last_active_at`; the list view
/// deduplicates per session taking the latest `last_active_at` (explicitly
/// in SQL — the query is correct whether or not the engine auto-dedups
/// same-PK rows). Because each row is a full snapshot, a touch can never
/// wipe immutable columns: there is no partial-row rewrite at all.
///
/// `title` is the user-assigned session name (manual, never auto-generated;
/// `NULL` = unnamed, the frontend shows the id). `pinned` is the user pin
/// flag (`NULL` = never touched, reads as unpinned; the list sorts pinned
/// sessions first). `archived` is the user archive flag (`NULL` = never
/// touched, reads as unarchived; archived sessions are hidden from the
/// default list and folded into the sidebar's archived group). `writer` is
/// the process identity of the process that wrote this snapshot row
/// (`pid@hostname#nonce`, see [`process_identity`]): an audit trail of who
/// wrote each snapshot, and a best-effort hint in concurrent-write
/// conflict errors.
///
/// Unlike Greptime's `TIME INDEX (last_active_at)`, the SQLite primary key
/// carries `last_active_at` as well, so two snapshots of the same session
/// written in the same microsecond collide loudly (a real INSERT error)
/// instead of silently overwriting. [`next_event_time_us`] is strictly
/// monotonic per process, so this can only ever happen across processes
/// sharing one database file.
const CREATE_TABLE_SESSIONS: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    workspace_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_active_at INTEGER NOT NULL,
    model TEXT NULL,
    "role" TEXT NULL,
    entry_count INTEGER NOT NULL DEFAULT 0,
    parent_session_id TEXT NULL,
    parent_task_id INTEGER NULL,
    title TEXT NULL,
    pinned INTEGER NULL,
    archived INTEGER NULL,
    writer TEXT NULL,
    PRIMARY KEY (workspace_id, session_id, last_active_at)
)
"#;

/// DDL for the token-usage statistics table. Idempotent. One row per model
/// call whose usage is worth accounting (regular turns, compactions, the
/// desktop-pet summarizer), scoped to (workspace, session) like
/// `session_entries`. The primary key carries only `(workspace_id,
/// session_id, seq)` (no `event_time_us`): `seq` is a strictly monotonic
/// per-process microsecond timestamp (see [`next_event_time_us`]) reused
/// as the row sequence, so within one process no two rows of the same
/// session can collide. Across processes sharing one database file a
/// same-microsecond write would collide loudly (a real INSERT error) — the
/// write is best-effort and the runner is single-writer per session, so
/// this is acceptable, exactly like the `sessions` snapshot PK.
///
/// The read path is a plain `GROUP BY session_id, model, kind` aggregate
/// ([`Self::usage_summary`]); no per-seq dedup is needed.
const CREATE_TABLE_USAGE: &str = r#"
CREATE TABLE IF NOT EXISTS usage_entries (
    workspace_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    event_time_us INTEGER NOT NULL,
    model TEXT NOT NULL,
    kind TEXT NOT NULL,
    input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, session_id, seq)
)
"#;

/// One `backfill_sessions` aggregate row:
/// `(session_id, entry_count, created_at_us, last_active_at_us)`.
type BackfillRow = (String, i64, i64, i64);

pub struct SqliteSession {
    /// The shared turso connection, guarded by an async mutex so every
    /// `&self` method can serialize its statements against concurrent
    /// callers (the store hands the session out behind a tokio Mutex, and
    /// the delegate's closure-based persistence path shares it further).
    conn: Arc<tokio::sync::Mutex<turso::Connection>>,
    /// Next sequence number for appends within this session. Interior
    /// mutability because every append path takes `&self` while the store
    /// hands the session out behind a tokio Mutex.
    next_seq: std::sync::Mutex<i64>,
    workspace_id: String,
    session_id: String,
    /// The session's latest metadata snapshot, cached at connect time so
    /// every touch carries the immutable columns (created_at/model/role/
    /// parent/title/pinned) without re-reading them. `None` = no row yet
    /// (brand-new session, or a subagent whose parent has not written its
    /// row yet).
    /// Interior mutability because every touch path takes `&self`.
    cached_meta: std::sync::Mutex<Option<SessionMeta>>,
}

/// Ensure the parent directory of the database file exists before turso
/// opens it, so a configured `path = "~/.local/share/e-agent/sessions.db"`-style
/// location works even when the directory has never been created (e.g. on
/// a fresh Windows install where `D:/.e-agent/` does not exist yet).
///
/// Idempotent: `create_dir_all` is a no-op once the directory exists, so
/// multiple workspaces sharing one database file each calling this at
/// connect time is fine — nothing is re-created. `:memory:` and bare file
/// names (`sessions.db` with no parent) are skipped.
fn ensure_db_parent_dir(db_path: &str) -> Result<(), String> {
    let path = Path::new(db_path);
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "cannot create parent directory '{}' for SQLite database '{db_path}': {e}",
                    parent.display()
                )
            })
        }
        _ => Ok(()),
    }
}

impl SqliteSession {
    /// Connect to the SQLite/turso database file and ensure the tables
    /// exist. `db_path` is a path to a SQLite-compatible database file
    /// (`:memory:` works for tests); `workspace_id` is derived from the
    /// canonical workspace root (see [`derive_workspace_id`]) and scopes
    /// sessions to their workspace; `session_id` binds this store to one
    /// session (like the Greptime backend, the store is per-session).
    ///
    /// Sets `PRAGMA journal_mode=WAL` (a no-op for `:memory:`) and
    /// `PRAGMA busy_timeout=5000` so processes sharing one file wait out
    /// short writer locks instead of failing, seeds `next_seq` from
    /// `MAX(seq)` and caches the session's latest metadata snapshot.
    pub async fn connect(
        db_path: &str,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<Self, String> {
        ensure_db_parent_dir(db_path)?;
        let db = turso::Builder::new_local(db_path)
            .build()
            .await
            .map_err(|e| format!("cannot open SQLite database '{db_path}': {e}"))?;
        let conn = db
            .connect()
            .map_err(|e| format!("cannot connect to SQLite database '{db_path}': {e}"))?;

        // `PRAGMA journal_mode=WAL` is a row-returning pragma (turso's
        // `execute` rejects statements that return rows; `:memory:` answers
        // "memory" and file databases switch to WAL). Drain the result row.
        {
            let mut rows = conn
                .query("PRAGMA journal_mode=WAL", ())
                .await
                .map_err(|e| format!("cannot set journal_mode=WAL on '{db_path}': {e}"))?;
            while rows
                .next()
                .await
                .map_err(|e| format!("cannot set journal_mode=WAL on '{db_path}': {e}"))?
                .is_some()
            {}
        }
        // Wait out short writer locks from other processes sharing the file
        // (e.g. a TUI + Web pair) instead of failing immediately.
        conn.busy_timeout(std::time::Duration::from_millis(5000))
            .map_err(|e| format!("cannot set busy_timeout on '{db_path}': {e}"))?;

        conn.execute(CREATE_TABLE, ())
            .await
            .map_err(|e| format!("cannot create session_entries table: {e}"))?;
        conn.execute(CREATE_TABLE_RUNNING_TASKS, ())
            .await
            .map_err(|e| format!("cannot create running_tasks table: {e}"))?;
        conn.execute(CREATE_TABLE_SESSIONS, ())
            .await
            .map_err(|e| format!("cannot create sessions table: {e}"))?;
        conn.execute(CREATE_TABLE_USAGE, ())
            .await
            .map_err(|e| format!("cannot create usage_entries table: {e}"))?;

        // Idempotent schema migration for the `title`, `pinned`,
        // `archived` and `writer` columns — a table-structure evolution:
        // the `sessions` table shipped without them, so pre-existing
        // databases need an ALTER (fresh databases already have them via
        // CREATE_TABLE_SESSIONS above; `CREATE TABLE IF NOT EXISTS` never
        // adds columns, so without this the shared column-bearing SELECTs
        // would hard-error on an old database file — the same failure
        // mode `pinned` would have hit, and which `archived` — a newer
        // column — definitely hits). There are no historical rows to
        // backfill: old rows simply read the columns back as NULL (the
        // read path treats them as `Option`).
        //
        // Mirrors the Greptime backend's probe-then-ALTER pattern
        // (session_greptime.rs): probe `PRAGMA table_info(sessions)` for
        // each column, `ALTER TABLE sessions ADD COLUMN` when missing
        // (the ALTER deliberately omits an explicit NULL constraint —
        // see the turso quirk note below). A failed migration must NOT
        // block the connection — if ALTER errors anyway we keep running
        // with the feature degraded: the meta-row cache below is skipped
        // and later column-bearing queries fail loudly with context;
        // transcript operations are unaffected.
        let mut title_available = true;
        let mut pinned_available = true;
        let mut archived_available = true;
        let mut writer_available = true;
        {
            let mut rows = conn
                .query("PRAGMA table_info(sessions)", ())
                .await
                .map_err(|e| format!("cannot inspect sessions table schema: {e}"))?;
            let mut columns: Vec<String> = Vec::new();
            while let Some(row) = rows
                .next()
                .await
                .map_err(|e| format!("cannot inspect sessions table schema: {e}"))?
            {
                // PRAGMA table_info columns: cid, name, type, notnull,
                // dflt_value, pk — the name is index 1.
                if let Some(name) = row
                    .get_value(1)
                    .map_err(|e| format!("cannot inspect sessions table schema: {e}"))?
                    .as_text()
                {
                    columns.push(name.clone());
                }
            }
            // Keep the added column types identical to CREATE_TABLE_SESSIONS
            // (TEXT for title/writer, INTEGER for the boolean flags) so a
            // migrated old database and a fresh database share one schema.
            // NOTE: turso's ALTER TABLE ADD COLUMN mis-parses an explicit
            // trailing `NULL` as NOT NULL (CREATE TABLE is unaffected —
            // fresh DBs keep nullable columns), so the ALTER must OMIT the
            // constraint; ADD COLUMN defaults to nullable anyway.
            let migrations: [(&str, &str, &str, &mut bool); 4] = [
                ("title", "TEXT", "session titles", &mut title_available),
                (
                    "pinned",
                    "INTEGER",
                    "session pinning",
                    &mut pinned_available,
                ),
                (
                    "archived",
                    "INTEGER",
                    "session archiving",
                    &mut archived_available,
                ),
                ("writer", "TEXT", "writer audit", &mut writer_available),
            ];
            for (name, sql_type, feature, available) in migrations {
                if columns.iter().any(|c| c == name) {
                    continue; // column already present (fresh DB or migrated)
                }
                let sql = format!("ALTER TABLE sessions ADD COLUMN {name} {sql_type}");
                if let Err(error) = conn.execute(&sql, ()).await {
                    *available = false;
                    eprintln!(
                        "e-agent: cannot add sessions.{name} column ({feature} unavailable): \
                         {error}"
                    );
                }
            }
        }

        // Same probe-then-ALTER migration for the `owner` column of the
        // `running_tasks` table (the process identity of the process that
        // started each background task, added after the table shipped).
        // Pre-existing databases need the ALTER; fresh databases already
        // have the column via CREATE_TABLE_RUNNING_TASKS above. Old rows
        // read the column back as NULL, which the liveness probe treats
        // as "alive" (conservative). A failed ALTER does NOT block the
        // connection: the feature degrades — `unfinished_owner_all_dead`
        // and `record_task_start` fail loudly with context, transcript
        // operations are unaffected (same philosophy as the sessions
        // migration above).
        {
            let mut rows = conn
                .query("PRAGMA table_info(running_tasks)", ())
                .await
                .map_err(|e| format!("cannot inspect running_tasks table schema: {e}"))?;
            let mut columns: Vec<String> = Vec::new();
            while let Some(row) = rows
                .next()
                .await
                .map_err(|e| format!("cannot inspect running_tasks table schema: {e}"))?
            {
                // PRAGMA table_info columns: cid, name, type, notnull,
                // dflt_value, pk — the name is index 1.
                if let Some(name) = row
                    .get_value(1)
                    .map_err(|e| format!("cannot inspect running_tasks table schema: {e}"))?
                    .as_text()
                {
                    columns.push(name.clone());
                }
            }
            if !columns.iter().any(|c| c == "owner_identity") {
                // NOTE: same turso quirk as the sessions migration — omit
                // the explicit NULL constraint; ADD COLUMN defaults to
                // nullable anyway.
                if let Err(error) = conn
                    .execute(
                        "ALTER TABLE running_tasks ADD COLUMN owner_identity TEXT",
                        (),
                    )
                    .await
                {
                    eprintln!(
                        "e-agent: cannot add running_tasks.owner_identity column \
                         (background-task owner liveness unavailable): {error}"
                    );
                }
            }
        }

        // Index for the batched subagent-label lookup: `/api/sessions`
        // resolves every subagent label with ONE `all_subagent_labels`
        // query instead of a per-subagent N+1, and without an index that
        // query is a full table scan. Idempotent; a failed CREATE INDEX
        // does NOT block the connection — the feature degrades back to the
        // per-item `label_for_subagent` loop (same philosophy as the ALTER
        // migrations above).
        if let Err(error) = conn
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_running_tasks_subagent \
                 ON running_tasks (workspace_id, subagent_session_id)",
                (),
            )
            .await
        {
            eprintln!(
                "e-agent: cannot create running_tasks subagent index \
                 (batched subagent-label lookup unavailable): {error}"
            );
        }

        // Cache the session's latest metadata snapshot (if any) so later
        // touches rewrite complete rows without re-reading immutable
        // columns. The table is append-only; the newest row per session
        // wins (ORDER BY last_active_at DESC LIMIT 1). Skipped when a
        // feature column is unavailable (failed migration): the cache
        // query references the columns and would error on an unmigrated
        // table, exactly like Greptime's degraded mode.
        let cached_meta =
            if title_available && pinned_available && archived_available && writer_available {
                let mut rows = conn
                    .query(
                        "SELECT created_at, last_active_at, model, \"role\", entry_count, \
                                parent_session_id, parent_task_id, title, pinned, archived, writer \
                         FROM sessions \
                         WHERE workspace_id = ?1 AND session_id = ?2 \
                         ORDER BY last_active_at DESC LIMIT 1",
                        (workspace_id, session_id),
                    )
                    .await
                    .map_err(|e| format!("cannot query session metadata: {e}"))?;
                match rows
                    .next()
                    .await
                    .map_err(|e| format!("cannot query session metadata: {e}"))?
                {
                    Some(row) => Some(row_to_meta(&row, session_id)?),
                    None => None,
                }
            } else {
                None
            };

        let session = Self {
            conn: Arc::new(tokio::sync::Mutex::new(conn)),
            next_seq: std::sync::Mutex::new(0),
            workspace_id: workspace_id.to_string(),
            session_id: session_id.to_string(),
            cached_meta: std::sync::Mutex::new(cached_meta),
        };

        // Seed next_seq from the DB's current max seq (see db_max_seq).
        let max_seq = session.db_max_seq().await?;
        *session.next_seq.lock().unwrap() = max_seq
            .checked_add(1)
            .ok_or_else(|| format!("max_seq overflow in connect for session '{session_id}'"))?;
        Ok(session)
    }

    /// Query the current DB max seq for this session
    /// (`COALESCE(MAX(seq), -1)` → `-1` for an empty partition). Scans the
    /// full session partition, acceptable because sessions are append-only
    /// and bounded by typical turn counts. Used to seed `next_seq` at
    /// connect and to detect concurrent writers before every append.
    async fn db_max_seq(&self) -> Result<i64, String> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT COALESCE(MAX(seq), -1) AS max_seq \
                 FROM session_entries \
                 WHERE workspace_id = ?1 AND session_id = ?2",
                (self.workspace_id.as_str(), self.session_id.as_str()),
            )
            .await
            .map_err(|e| format!("cannot query max seq: {e}"))?;
        let row = rows
            .next()
            .await
            .map_err(|e| format!("cannot query max seq: {e}"))?
            .ok_or_else(|| "cannot query max seq: no row returned".to_string())?;
        row.get_value(0)
            .map_err(|e| format!("cannot query max seq: {e}"))?
            .as_integer()
            .copied()
            .ok_or_else(|| "cannot query max seq: not an integer".to_string())
    }

    /// Advance next_seq from a snapshot length (the number of contiguous
    /// seq 0..N entries that were just loaded).
    ///
    /// - Rejects `len < current next_seq` (rewind / monotonicity violation).
    /// - Rejects lengths that overflow `i64` (impossible for a real session).
    /// - The caller must have verified that the loaded snapshot is a
    ///   complete 0..N range so that `len` is the correct next seq
    ///   (importer TOCTOU re-read pattern).
    pub fn advance_next_seq_from_snapshot_len(&self, len: usize) -> Result<(), String> {
        let next: i64 = i64::try_from(len)
            .map_err(|_| "snapshot length overflowed i64 (impossible for a real session)")?;
        let mut guard = self.next_seq.lock().unwrap();
        if next < *guard {
            return Err(format!(
                "next_seq rewind: current={}, requested={}; \
                 monotonic advance only (use snapshot length, not DB row count)",
                *guard, next,
            ));
        }
        *guard = next;
        Ok(())
    }

    /// The session's live entry count (`next_seq` = `MAX(seq)+1`,
    /// maintained in memory on every append). Cheap metadata for the
    /// sessions list — no DB query, unlike the sessions-table
    /// `entry_count` snapshot which only refreshes on touch.
    pub fn entry_count(&self) -> i64 {
        *self.next_seq.lock().unwrap()
    }

    /// Load all entries for this session, deduplicated by seq (latest
    /// event_time wins) and sorted by winning event_time ASC, then seq ASC.
    /// Delegates to [`Self::load_with_seq`].
    pub async fn load(&self) -> Result<Vec<SessionEntry>, String> {
        self.load_with_seq()
            .await
            .map(|v| v.into_iter().map(|(_, e)| e).collect())
    }

    /// Load entries paired with their sequence numbers, deduplicated by
    /// seq (latest event_time wins per seq) and sorted by winning event_time
    /// ASC, then seq ASC. The seq remains available as identity and integrity
    /// metadata.
    ///
    /// **Duplicate handling per seq**:
    ///
    /// 1. Among all physical rows for a given seq, only the row(s) with
    ///    the **latest** `event_time` are retained. Older rows are silently
    ///    discarded (they are considered overwritten).
    ///
    /// 2. If the latest event_time has **multiple** physical rows (same
    ///    seq, same max event_time), their payloads are deserialised and
    ///    compared. If all are identical (idempotent retry at the same
    ///    microsecond), they are folded into one logical entry. If **any**
    ///    differ, this returns an error with the seq, event_time, session
    ///    id, and manual-inspection guidance.
    ///
    /// 3. Output is ordered by the winning event_time ASC, then seq ASC.
    pub async fn load_with_seq(&self) -> Result<Vec<(i64, SessionEntry)>, String> {
        let raw = self
            .query_raw_entries(
                "SELECT seq, event_time_us, payload FROM session_entries \
                 WHERE workspace_id = ?1 AND session_id = ?2 \
                 ORDER BY event_time_us ASC, seq ASC",
            )
            .await?;
        dedup_raw_entries(&raw, &self.session_id, &self.workspace_id, "event_time_us")
    }

    /// Run an entry-payload SELECT (first three columns seq, event_time_us,
    /// payload) with the bound session's workspace_id/session_id as the
    /// first two parameters and map every row to the dedup input tuple
    /// `(seq, NaiveDateTime, payload)`.
    async fn query_raw_entries(
        &self,
        sql: &str,
    ) -> Result<Vec<(i64, chrono::NaiveDateTime, String)>, String> {
        self.query_raw_entries_with(sql, &[]).await
    }

    /// Like [`Self::query_raw_entries`] but with extra bound parameters
    /// appended after the workspace_id/session_id pair (`extra` items are
    /// bound as `?3`, `?4`, … in order).
    async fn query_raw_entries_with(
        &self,
        sql: &str,
        extra: &[&i64],
    ) -> Result<Vec<(i64, chrono::NaiveDateTime, String)>, String> {
        let conn = self.conn.lock().await;
        let mut params: Vec<turso::Value> = Vec::with_capacity(2 + extra.len());
        params.push(turso::Value::Text(self.workspace_id.clone()));
        params.push(turso::Value::Text(self.session_id.clone()));
        for v in extra {
            params.push(turso::Value::Integer(**v));
        }
        let mut rows = conn
            .query(sql, turso::params_from_iter(params))
            .await
            .map_err(|e| format!("cannot load session entries: {e}"))?;
        let mut raw = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| format!("cannot load session entries: {e}"))?
        {
            let seq = row
                .get_value(0)
                .map_err(|e| format!("cannot load session entries: {e}"))?
                .as_integer()
                .copied()
                .ok_or_else(|| "cannot load session entries: seq is not an integer".to_string())?;
            let et_us = row
                .get_value(1)
                .map_err(|e| format!("cannot load session entries: {e}"))?
                .as_integer()
                .copied()
                .ok_or_else(|| {
                    "cannot load session entries: event_time is not an integer".to_string()
                })?;
            let payload = row
                .get_value(2)
                .map_err(|e| format!("cannot load session entries: {e}"))?
                .as_text()
                .cloned()
                .ok_or_else(|| "cannot load session entries: payload is not text".to_string())?;
            raw.push((seq, us_to_datetime(et_us), payload));
        }
        Ok(raw)
    }

    /// Load only the newest compaction segment: the last `Compaction` entry
    /// and everything after it. This is the segment the agent context
    /// actually needs on resume (`Agent::context` sends only the last
    /// compaction's summary + retained + everything after it), so loading
    /// just this slice keeps startup cheap on very long sessions.
    ///
    /// Segment boundary semantics (shared with [`Self::load_older`]):
    ///
    /// - head = `[last_comp, ∞)` — the last compaction row rides with its
    ///   following segment;
    /// - `load_older(before)` = `[prev_comp, before)` — the compaction row
    ///   that opens a segment rides with that segment;
    /// - the oldest segment = `[0, before)` and `load_older` returns `None`
    ///   as its cursor, meaning nothing older exists.
    ///
    /// Every row is loaded exactly once across the head + older chain.
    ///
    /// When the session has no compaction entries at all, the whole session
    /// is a single head segment and the full [`Self::load`] result is
    /// returned (behavior identical to the pre-segmentation load).
    pub async fn load_head(&self) -> Result<Vec<SessionEntry>, String> {
        let last_comp = self.last_compaction_seq(None).await?;
        let Some(last_comp) = last_comp else {
            return self.load().await;
        };

        let raw = self
            .query_raw_entries_with(
                "SELECT seq, event_time_us, payload FROM session_entries \
                 WHERE workspace_id = ?1 AND session_id = ?2 AND seq >= ?3 \
                 ORDER BY event_time_us ASC, seq ASC",
                &[&last_comp],
            )
            .await?;

        let entries =
            dedup_raw_entries(&raw, &self.session_id, &self.workspace_id, "event_time_us")?
                .into_iter()
                .map(|(_, e)| e)
                .collect();
        Ok(entries)
    }

    /// The backend seq of the newest compaction entry — the first seq of
    /// the head segment returned by [`Self::load_head`]. `None` when the
    /// session has no compaction at all, i.e. the whole session is one
    /// head segment and there is nothing older to load. The TUI uses this
    /// to seed the first [`Self::load_older`] call:
    /// `load_older(head_seq)` fetches the segment immediately before the
    /// head.
    pub async fn head_seq(&self) -> Result<Option<i64>, String> {
        self.last_compaction_seq(None).await
    }

    /// `MAX(seq)` of the newest compaction entry with `seq < before`
    /// (`None` = no upper bound), i.e. the last compaction overall.
    async fn last_compaction_seq(&self, before: Option<i64>) -> Result<Option<i64>, String> {
        let conn = self.conn.lock().await;
        let mut rows = match before {
            Some(before) => conn
                .query(
                    "SELECT MAX(seq) AS max_seq FROM session_entries \
                     WHERE workspace_id = ?1 AND session_id = ?2 \
                     AND entry_kind = 'compaction' AND seq < ?3",
                    (self.workspace_id.as_str(), self.session_id.as_str(), before),
                )
                .await
                .map_err(|e| format!("cannot query last compaction seq: {e}"))?,
            None => conn
                .query(
                    "SELECT MAX(seq) AS max_seq FROM session_entries \
                     WHERE workspace_id = ?1 AND session_id = ?2 AND entry_kind = 'compaction'",
                    (self.workspace_id.as_str(), self.session_id.as_str()),
                )
                .await
                .map_err(|e| format!("cannot query last compaction seq: {e}"))?,
        };
        let row = rows
            .next()
            .await
            .map_err(|e| format!("cannot query last compaction seq: {e}"))?
            .ok_or_else(|| "cannot query last compaction seq: no row".to_string())?;
        let v = row
            .get_value(0)
            .map_err(|e| format!("cannot query last compaction seq: {e}"))?;
        // MAX(seq) over no rows is NULL.
        if v.is_null() {
            return Ok(None);
        }
        v.as_integer()
            .copied()
            .map(Some)
            .ok_or_else(|| "cannot query last compaction seq: not an integer".to_string())
    }

    /// Load the compaction segment immediately older than `before_seq`:
    /// seq ∈ `[prev_comp, before)` where `prev_comp` is the newest
    /// compaction with `seq < before_seq`. The compaction row that opens
    /// the segment rides with it.
    ///
    /// `limit` enables fixed-size intra-segment paging: when `Some(n)`, the
    /// `n` entries closest to `before_seq` (the segment's tail, i.e. the
    /// newest of the segment) are returned instead of the whole segment,
    /// and the cursor is the seq of the oldest entry of the page — feed it
    /// back as the next `before_seq` to fetch the next older page *within
    /// the same segment*. This keeps the wire size bounded (~`limit` rows)
    /// on very long sessions instead of shipping a whole compaction segment
    /// (potentially hundreds of entries) per request. `n == 0` is treated
    /// as "no limit" (whole segment).
    ///
    /// Returns `(entries, cursor)` where:
    ///
    /// - `limit = None`: `cursor` is `Some(prev_comp)` when an older
    ///   segment exists (pass it as the next `before_seq`) and `None` when
    ///   this is the oldest segment (`[0, before)` — the full remaining
    ///   history was returned and nothing older exists).
    /// - `limit = Some(n)`: `cursor` is `Some(oldest page seq)` to continue
    ///   paging. When a page reaches the segment's opening compaction row
    ///   (`prev_comp`) the cursor is `Some(prev_comp)` — the next call
    ///   crosses into the older segment, exactly like the `limit = None`
    ///   chain. The cursor is `None` only when the page reaches seq `0` /
    ///   the oldest segment: nothing older exists.
    ///
    /// So the cursor contract is identical with or without a limit:
    /// `next_before_seq = null` ⇔ no older entries exist, and every row is
    /// loaded exactly once across the head + older chain.
    ///
    /// Segment boundary semantics (shared with [`Self::load_head`]):
    ///
    /// - head = `[last_comp, ∞)`;
    /// - `load_older(before)` = `[prev_comp, before)`;
    /// - the oldest segment = `[0, before)`.
    ///
    /// seq is not necessarily contiguous (compaction rows, retried writes),
    /// so paging uses `seq >= start AND seq < before_seq ORDER BY seq DESC
    /// LIMIT n` (the tail of the segment) and reverses the page to
    /// ascending order. On the paged path the limit applies AFTER a
    /// SQL-side dedup (latest `event_time_us` per seq, a subquery join —
    /// `MAX(event_time_us)` GROUP BY seq) so same-seq duplicate physical
    /// rows from retried writes never consume limit slots: every page is
    /// exactly `min(n, remaining distinct seqs)` entries and the cursor
    /// chain covers each seq exactly once.
    pub async fn load_older(
        &self,
        before_seq: i64,
        limit: Option<usize>,
    ) -> Result<(Vec<SessionEntry>, Option<i64>), String> {
        let prev_comp = self.last_compaction_seq(Some(before_seq)).await?;
        let page = limit.filter(|&n| n > 0);

        let mut raw: Vec<(i64, chrono::NaiveDateTime, String)> = match prev_comp {
            // Middle segment: [prev_comp, before).
            Some(prev) => {
                let conn = self.conn.lock().await;
                let rows = if let Some(n) = page {
                    // Dedup by seq (latest event_time_us wins) IN SQL
                    // before LIMIT: same-seq duplicate physical rows
                    // (retried writes) must not consume limit slots, or a
                    // page could come back smaller than `limit` after the
                    // Rust-side dedup and the cursor chain could skip or
                    // repeat entries. The join keeps ties (same seq, same
                    // max event_time) for the Rust-side conflict check.
                    conn.query(
                        "SELECT t.seq, t.event_time_us, t.payload FROM session_entries t \
                         JOIN ( \
                             SELECT seq, MAX(event_time_us) AS me FROM session_entries \
                             WHERE workspace_id = ?1 AND session_id = ?2 \
                             AND seq >= ?3 AND seq < ?4 \
                             GROUP BY seq \
                         ) g ON t.seq = g.seq AND t.event_time_us = g.me \
                         WHERE t.workspace_id = ?1 AND t.session_id = ?2 \
                         ORDER BY t.seq DESC LIMIT ?5",
                        (
                            self.workspace_id.as_str(),
                            self.session_id.as_str(),
                            prev,
                            before_seq,
                            n as i64,
                        ),
                    )
                    .await
                    .map_err(|e| format!("cannot load middle segment page: {e}"))?
                } else {
                    conn.query(
                        "SELECT seq, event_time_us, payload FROM session_entries \
                         WHERE workspace_id = ?1 AND session_id = ?2 \
                         AND seq >= ?3 AND seq < ?4 \
                         ORDER BY event_time_us ASC, seq ASC",
                        (
                            self.workspace_id.as_str(),
                            self.session_id.as_str(),
                            prev,
                            before_seq,
                        ),
                    )
                    .await
                    .map_err(|e| format!("cannot load middle segment: {e}"))?
                };
                rows_to_raw(rows).await?
            }
            // Oldest segment: [0, before). Nothing older follows.
            None => {
                let conn = self.conn.lock().await;
                let rows = if let Some(n) = page {
                    // Same SQL-side dedup-before-LIMIT as the middle
                    // segment: limit counts DISTINCT seqs only.
                    conn.query(
                        "SELECT t.seq, t.event_time_us, t.payload FROM session_entries t \
                         JOIN ( \
                             SELECT seq, MAX(event_time_us) AS me FROM session_entries \
                             WHERE workspace_id = ?1 AND session_id = ?2 AND seq < ?3 \
                             GROUP BY seq \
                         ) g ON t.seq = g.seq AND t.event_time_us = g.me \
                         WHERE t.workspace_id = ?1 AND t.session_id = ?2 \
                         ORDER BY t.seq DESC LIMIT ?4",
                        (
                            self.workspace_id.as_str(),
                            self.session_id.as_str(),
                            before_seq,
                            n as i64,
                        ),
                    )
                    .await
                    .map_err(|e| format!("cannot load oldest segment page: {e}"))?
                } else {
                    conn.query(
                        "SELECT seq, event_time_us, payload FROM session_entries \
                         WHERE workspace_id = ?1 AND session_id = ?2 AND seq < ?3 \
                         ORDER BY event_time_us ASC, seq ASC",
                        (
                            self.workspace_id.as_str(),
                            self.session_id.as_str(),
                            before_seq,
                        ),
                    )
                    .await
                    .map_err(|e| format!("cannot load oldest segment: {e}"))?
                };
                rows_to_raw(rows).await?
            }
        };

        // Paged fetch: rows came back newest-first (ORDER BY seq DESC);
        // flip to ascending and derive the next cursor = the oldest seq of
        // the page. `Some(seq)` keeps paging (within the segment, or across
        // the segment boundary when seq == prev_comp); `None` only when the
        // page reaches seq 0 — the true start of the session.
        let cursor = if page.is_some() {
            let oldest = raw.last().map(|(seq, _, _)| *seq).filter(|&seq| seq > 0);
            raw.reverse();
            oldest
        } else {
            prev_comp
        };

        let entries =
            dedup_raw_entries(&raw, &self.session_id, &self.workspace_id, "event_time_us")?
                .into_iter()
                .map(|(_, e)| e)
                .collect();
        Ok((entries, cursor))
    }

    /// Load the oldest compaction segment: everything before the first
    /// `Compaction` entry — seq ∈ `[0, first_comp)`. The oldest segment
    /// never contains a compaction row itself.
    ///
    /// Returns `(entries, cursor)` where `cursor` is `Some(first_comp_seq)`
    /// when the session has compaction entries — pass it as the first
    /// `after_seq` to [`Self::load_newer`] — and `None` when the session
    /// has no compaction at all (the whole session is a single head
    /// segment, already loaded by [`Self::load_head`], and there is
    /// nothing older or in between to load).
    ///
    /// Segment boundary semantics (shared with [`Self::load_head`],
    /// [`Self::load_older`] and [`Self::load_newer`]):
    ///
    /// - oldest = `[0, first_comp)`;
    /// - `load_newer(after)` = `[after, next_comp)` — the compaction row
    ///   that opens a segment rides with that segment;
    /// - head = `[last_comp, ∞)` (loaded by [`Self::load_head`];
    ///   `load_newer` never returns the head segment).
    ///
    /// Every row is loaded exactly once across the oldest + newer + head
    /// chain.
    pub async fn load_oldest(&self) -> Result<(Vec<SessionEntry>, Option<i64>), String> {
        let first_comp: Option<i64> = {
            let conn = self.conn.lock().await;
            let mut rows = conn
                .query(
                    "SELECT MIN(seq) AS min_seq FROM session_entries \
                     WHERE workspace_id = ?1 AND session_id = ?2 AND entry_kind = 'compaction'",
                    (self.workspace_id.as_str(), self.session_id.as_str()),
                )
                .await
                .map_err(|e| format!("cannot query first compaction seq: {e}"))?;
            let row = rows
                .next()
                .await
                .map_err(|e| format!("cannot query first compaction seq: {e}"))?
                .ok_or_else(|| "cannot query first compaction seq: no row".to_string())?;
            let v = row
                .get_value(0)
                .map_err(|e| format!("cannot query first compaction seq: {e}"))?;
            // MIN(seq) over no rows is NULL.
            if v.is_null() {
                None
            } else {
                v.as_integer().copied().map(Some).ok_or_else(|| {
                    "cannot query first compaction seq: not an integer".to_string()
                })?
            }
        };
        let Some(first_comp) = first_comp else {
            // No compaction at all: the head segment covers the whole
            // session, so there is nothing older to load.
            return Ok((Vec::new(), None));
        };

        let raw = self
            .query_raw_entries_with(
                "SELECT seq, event_time_us, payload FROM session_entries \
                 WHERE workspace_id = ?1 AND session_id = ?2 AND seq < ?3 \
                 ORDER BY event_time_us ASC, seq ASC",
                &[&first_comp],
            )
            .await?;

        let entries =
            dedup_raw_entries(&raw, &self.session_id, &self.workspace_id, "event_time_us")?
                .into_iter()
                .map(|(_, e)| e)
                .collect();
        Ok((entries, Some(first_comp)))
    }

    /// Load the compaction segment immediately newer than `after_seq`:
    /// seq ∈ `[after, next_comp)` where `next_comp` is the oldest
    /// compaction with `seq > after_seq`. The compaction row at
    /// `after_seq` rides with the returned segment (a segment opens with
    /// its compaction).
    ///
    /// Returns `(entries, cursor)` where `cursor` is `Some(next_comp)`
    /// when another middle segment exists (pass it as the next
    /// `after_seq`) and `None` when no compaction follows `after_seq` —
    /// the head segment (`[last_comp, ∞)`, loaded by [`Self::load_head`])
    /// has been reached and there is nothing new to fetch. `load_newer`
    /// never returns the head segment itself; the caller already holds it.
    ///
    /// Cursor chain: [`Self::load_oldest`] returns the first compaction
    /// seq; feeding it here walks the middle segments forward (first →
    /// next → … → last), ending with `None` right before the head.
    pub async fn load_newer(
        &self,
        after_seq: i64,
    ) -> Result<(Vec<SessionEntry>, Option<i64>), String> {
        let next_comp: Option<i64> = {
            let conn = self.conn.lock().await;
            let mut rows = conn
                .query(
                    "SELECT MIN(seq) AS min_seq FROM session_entries \
                     WHERE workspace_id = ?1 AND session_id = ?2 \
                     AND entry_kind = 'compaction' AND seq > ?3",
                    (
                        self.workspace_id.as_str(),
                        self.session_id.as_str(),
                        after_seq,
                    ),
                )
                .await
                .map_err(|e| format!("cannot query next compaction seq: {e}"))?;
            let row = rows
                .next()
                .await
                .map_err(|e| format!("cannot query next compaction seq: {e}"))?
                .ok_or_else(|| "cannot query next compaction seq: no row".to_string())?;
            let v = row
                .get_value(0)
                .map_err(|e| format!("cannot query next compaction seq: {e}"))?;
            // MIN(seq) over no rows is NULL.
            if v.is_null() {
                None
            } else {
                v.as_integer()
                    .copied()
                    .map(Some)
                    .ok_or_else(|| "cannot query next compaction seq: not an integer".to_string())?
            }
        };

        let raw: Vec<(i64, chrono::NaiveDateTime, String)> = match next_comp {
            // Middle segment: [after, next_comp).
            Some(next) => {
                self.query_raw_entries_with(
                    "SELECT seq, event_time_us, payload FROM session_entries \
                     WHERE workspace_id = ?1 AND session_id = ?2 \
                     AND seq >= ?3 AND seq < ?4 \
                     ORDER BY event_time_us ASC, seq ASC",
                    &[&after_seq, &next],
                )
                .await?
            }
            // Nothing newer: the caller already holds the head segment.
            None => Vec::new(),
        };

        let entries =
            dedup_raw_entries(&raw, &self.session_id, &self.workspace_id, "event_time_us")?
                .into_iter()
                .map(|(_, e)| e)
                .collect();
        Ok((entries, next_comp))
    }

    /// Append new entries atomically per multi-row INSERT statement.
    ///
    /// SQLite supports real transactions, but atomicity is per statement
    /// here to mirror the Greptime contract exactly: a single multi-row
    /// INSERT either commits all N rows or commits zero. Serialization
    /// happens before any DB write.
    ///
    /// `next_seq` is computed once before the INSERT. On failure
    /// `next_seq` is unchanged, so retries of the same slice reuse the same
    /// seq range. Identical-payload duplicates from a
    /// fully-committed-then-retried batch are folded by the read path.
    ///
    /// **Concurrent-write detection**: before any DB write the current DB
    /// max seq is re-read (see [`Self::db_max_seq`]) and compared against
    /// our cursor. A second writer (TUI + Web window, two e-agent processes
    /// sharing one database file, import_jsonl) that committed rows at or
    /// above our `base_seq` is otherwise invisible — both writers would
    /// resume from their own `next_seq` and silently interleave. Detection
    /// is best-effort: it closes the pre-write window but cannot be atomic
    /// (the check and the INSERT are separate statements), so a write
    /// racing the check itself can still slip through. The overlap
    /// comparison distinguishes "our own earlier commit" (idempotent
    /// retry: payloads identical → skip re-insertion) from "a foreign
    /// writer" (any payload mismatch → conflict error).
    pub async fn append(&self, entries: &[SessionEntry]) -> Result<(), String> {
        if entries.is_empty() {
            return Ok(());
        }
        let n = entries.len();

        // Validate the complete write range before any DB write.
        // All seqs from base_seq..base_seq+n must be representable as i64
        // and the final cursor must also be in range.
        let base_seq = *self.next_seq.lock().unwrap();
        let n_i64 = i64::try_from(n).map_err(|_| "append count overflowed i64")?;
        let final_cursor = base_seq.checked_add(n_i64).ok_or_else(|| {
            format!(
                "seq range overflow: base_seq={base_seq} count={n}; \
                 max seq would exceed i64::MAX"
            )
        })?;

        // Serialize all entries upfront so serialization errors happen
        // before any database write.
        let mut prepped: Vec<(i64, i64, String, String, bool)> = Vec::with_capacity(n);
        for (i, entry) in entries.iter().enumerate() {
            let seq = base_seq + i as i64;
            let payload = serde_json::to_string(entry)
                .map_err(|e| format!("cannot serialize entry seq {seq}: {e}"))?;
            let kind = entry_kind(entry).to_string();
            let err = is_error(entry);
            let ts = next_event_time_us();
            prepped.push((seq, ts, kind, payload, err));
        }

        // Concurrent-write detection: re-read the DB max seq before any
        // write. db_max < base_seq → nothing above our cursor, normal path.
        // db_max >= base_seq → rows already exist in [base_seq, db_max];
        // fold the overlapping part and compare with this batch.
        let db_max = self.db_max_seq().await?;
        if db_max >= base_seq {
            let overlap_hi = db_max.min(final_cursor - 1);
            let overlap_len = (overlap_hi - base_seq + 1) as usize;
            let conflict_seq = self
                .overlap_conflict_seq(&prepped, base_seq, overlap_hi)
                .await?;
            if let Some(seq) = conflict_seq {
                // Best-effort writer hint for the error message: read the
                // latest metadata snapshot's writer from the sessions audit
                // table. The hint is labeled "latest metadata writer"
                // because snapshot timing is not guaranteed to match the
                // conflicting append exactly — two writers could in theory
                // touch within the same microsecond, and snapshots never
                // overwrite (append-only). In practice, though, the latest
                // writer is almost always the adversary: the losing writer
                // B's touch runs after its own (conflicting) append, so B
                // re-stamps the row with B's identity after A's last
                // snapshot. A failed or empty lookup falls back to the
                // plain message (the `concurrent write conflict` substring
                // is preserved — `friendly_failure` depends on it).
                let writer_hint = self.latest_meta_writer().await;
                return Err(format_conflict_error(
                    &self.session_id,
                    db_max,
                    base_seq,
                    seq,
                    writer_hint.as_deref(),
                ));
            }
            // The whole overlap matches this batch: it is our own earlier
            // commit (committed-then-errored append whose next_seq never
            // advanced). Treat it as already written.
            if overlap_len == n {
                // The DB may have advanced beyond our batch (foreign rows);
                // resume past the true max so we never reuse foreign seqs.
                let mut guard = self.next_seq.lock().unwrap();
                *guard = db_max.checked_add(1).ok_or_else(|| {
                    format!(
                        "max_seq overflow advancing next_seq after idempotent \
                         retry in session '{}'",
                        self.session_id
                    )
                })?;
                return Ok(());
            }
            // Partial overlap: the matching prefix stays committed; insert
            // only the remainder. prepped seqs are base_seq+i, so
            // prepped[overlap_len..] continues contiguously from db_max+1
            // and needs no re-serialization.
            self.insert_prepped(&prepped[overlap_len..]).await?;
            *self.next_seq.lock().unwrap() = final_cursor;
            return Ok(());
        }

        self.insert_prepped(&prepped).await?;

        // Advance next_seq only after the insert succeeds, so a partial
        // failure does not shift sequence numbers on retry.
        *self.next_seq.lock().unwrap() = final_cursor;

        Ok(())
    }

    /// Insert one token-usage row into the `usage_entries` table. `kind`
    /// is one of "regular" | "compact" | "summarizer". `seq` reuses the
    /// strictly monotonic per-process microsecond clock (see
    /// [`next_event_time_us`]), which doubles as the row's `event_time_us`
    /// — no separate per-session usage seq state is needed, and the
    /// primary key `(workspace_id, session_id, seq)` stays collision-free
    /// within one process (see the `CREATE_TABLE_USAGE` comment).
    ///
    /// `workspace_id`/`session_id` are explicit parameters (not the bound
    /// session's) so the workspace-scoped meta store can record usage for
    /// any session id, and the store facade passes them through.
    pub async fn append_usage(
        &self,
        workspace_id: &str,
        session_id: &str,
        model: &str,
        kind: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<(), String> {
        let seq = next_event_time_us();
        self.conn
            .lock()
            .await
            .execute(
                "INSERT INTO usage_entries \
                 (workspace_id, session_id, seq, event_time_us, model, kind, input_tokens, output_tokens) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                (
                    workspace_id,
                    session_id,
                    seq,
                    seq,
                    model,
                    kind,
                    input_tokens as i64,
                    output_tokens as i64,
                ),
            )
            .await
            .map_err(|e| format!("cannot insert token usage: {e}"))?;
        Ok(())
    }

    /// Aggregate token usage per (session_id, model, kind) for this
    /// workspace: totals plus the first/last event timestamps (µs since
    /// epoch) of each group, newest activity first.
    pub async fn usage_summary(&self) -> Result<Vec<UsageRow>, String> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT session_id, model, kind, \
                        SUM(input_tokens) AS input_tokens, SUM(output_tokens) AS output_tokens, \
                        MIN(event_time_us) AS first_ts, MAX(event_time_us) AS last_ts \
                 FROM usage_entries \
                 WHERE workspace_id = ?1 \
                 GROUP BY session_id, model, kind \
                 ORDER BY last_ts DESC",
                (self.workspace_id.as_str(),),
            )
            .await
            .map_err(|e| format!("cannot query token usage: {e}"))?;
        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| format!("cannot query token usage: {e}"))?
        {
            let text_at = |index: usize, label: &str| {
                row.get_value(index)
                    .map_err(|e| format!("cannot query token usage: {e}"))?
                    .as_text()
                    .cloned()
                    .ok_or_else(|| format!("cannot query token usage: {label} is not text"))
            };
            let int_at = |index: usize, label: &str| {
                row.get_value(index)
                    .map_err(|e| format!("cannot query token usage: {e}"))?
                    .as_integer()
                    .copied()
                    .ok_or_else(|| format!("cannot query token usage: {label} is not an integer"))
            };
            out.push(UsageRow {
                session_id: text_at(0, "session_id")?,
                model: text_at(1, "model")?,
                kind: text_at(2, "kind")?,
                input_tokens: u64::try_from(int_at(3, "input_tokens")?).map_err(|_| {
                    "cannot query token usage: input_tokens is negative".to_string()
                })?,
                output_tokens: u64::try_from(int_at(4, "output_tokens")?).map_err(|_| {
                    "cannot query token usage: output_tokens is negative".to_string()
                })?,
                first_ts: int_at(5, "first_ts")?,
                last_ts: int_at(6, "last_ts")?,
            });
        }
        Ok(out)
    }

    /// Aggregate token usage per (session_id, model, kind) restricted to
    /// the given session ids (the session itself plus its subagent
    /// children, for the web UI's persisted usage line). Same row shape as
    /// [`usage_summary`]; an empty `session_ids` slice short-circuits to
    /// an empty vector (no query).
    pub async fn usage_for_sessions(
        &self,
        session_ids: &[String],
    ) -> Result<Vec<UsageRow>, String> {
        if session_ids.is_empty() {
            return Ok(Vec::new());
        }
        // 占位符数量随会话数展开（绑定参数，无注入面）；同会话+子会话
        // 只有个位数 id，语句很短。
        let placeholders = vec!["?"; session_ids.len()].join(", ");
        let sql = format!(
            "SELECT session_id, model, kind, \
                    SUM(input_tokens) AS input_tokens, SUM(output_tokens) AS output_tokens, \
                    MIN(event_time_us) AS first_ts, MAX(event_time_us) AS last_ts \
             FROM usage_entries \
             WHERE workspace_id = ?1 AND session_id IN ({placeholders}) \
             GROUP BY session_id, model, kind \
             ORDER BY last_ts DESC"
        );
        let mut params: Vec<turso::Value> = vec![turso::Value::Text(self.workspace_id.clone())];
        for id in session_ids {
            params.push(turso::Value::Text(id.clone()));
        }
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(&sql, turso::params_from_iter(params))
            .await
            .map_err(|e| format!("cannot query token usage: {e}"))?;
        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| format!("cannot query token usage: {e}"))?
        {
            let text_at = |index: usize, label: &str| {
                row.get_value(index)
                    .map_err(|e| format!("cannot query token usage: {e}"))?
                    .as_text()
                    .cloned()
                    .ok_or_else(|| format!("cannot query token usage: {label} is not text"))
            };
            let int_at = |index: usize, label: &str| {
                row.get_value(index)
                    .map_err(|e| format!("cannot query token usage: {e}"))?
                    .as_integer()
                    .copied()
                    .ok_or_else(|| format!("cannot query token usage: {label} is not an integer"))
            };
            out.push(UsageRow {
                session_id: text_at(0, "session_id")?,
                model: text_at(1, "model")?,
                kind: text_at(2, "kind")?,
                input_tokens: u64::try_from(int_at(3, "input_tokens")?).map_err(|_| {
                    "cannot query token usage: input_tokens is negative".to_string()
                })?,
                output_tokens: u64::try_from(int_at(4, "output_tokens")?).map_err(|_| {
                    "cannot query token usage: output_tokens is negative".to_string()
                })?,
                first_ts: int_at(5, "first_ts")?,
                last_ts: int_at(6, "last_ts")?,
            });
        }
        Ok(out)
    }

    /// Insert prepped rows in one multi-row INSERT. SQLite has no
    /// 65535-bound-parameter limit (and real turns are far below any
    /// variable-count limit), so — unlike the Greptime backend — a single
    /// statement covers the whole batch and is atomic as a whole.
    async fn insert_prepped(
        &self,
        prepped: &[(i64, i64, String, String, bool)],
    ) -> Result<(), String> {
        let sql = build_multi_row_insert(prepped.len());
        let wid = &self.workspace_id;
        let sid = &self.session_id;
        let mut values: Vec<turso::Value> = Vec::with_capacity(prepped.len() * 7);
        for (seq, ts, kind, payload, err) in prepped {
            values.push(turso::Value::Text(wid.clone()));
            values.push(turso::Value::Text(sid.clone()));
            values.push(turso::Value::Integer(*seq));
            values.push(turso::Value::Integer(*ts));
            values.push(turso::Value::Text(kind.clone()));
            values.push(turso::Value::Text(payload.clone()));
            values.push(turso::Value::Integer(if *err { 1 } else { 0 }));
        }
        let conn = self.conn.lock().await;
        conn.execute(&sql, turso::params_from_iter(values))
            .await
            .map_err(|e| format!("cannot append to session_entries: {e}"))?;
        Ok(())
    }

    /// The `writer` of the latest metadata snapshot for the bound session
    /// (best-effort hint for conflict errors, see [`Self::append`]).
    async fn latest_meta_writer(&self) -> Option<String> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT writer FROM sessions \
                 WHERE workspace_id = ?1 AND session_id = ?2 \
                 ORDER BY last_active_at DESC LIMIT 1",
                (self.workspace_id.as_str(), self.session_id.as_str()),
            )
            .await
            .ok()?;
        let row = rows.next().await.ok()??;
        row.get_value(0).ok()?.as_text().cloned()
    }

    /// Compare the DB rows already present in the overlapping seq window
    /// `[base_seq, overlap_hi]` against this batch's prepped rows for the
    /// same seqs.
    ///
    /// Folds per seq to the latest `event_time` (matching the read path's
    /// dedup semantics) and compares the winning payload against the
    /// prepped payload:
    ///
    /// - All match → our own earlier commit (idempotent retry) → `Ok(None)`.
    /// - Any payload differs → a foreign writer → `Ok(Some(seq))` with the
    ///   first divergent seq.
    ///
    /// A window with missing seqs (fewer rows than the window width) is
    /// also reported as a conflict: our own commits are contiguous
    /// multi-row INSERTs, so a hole can only have been left by a foreign
    /// writer that advanced seqs without writing them.
    async fn overlap_conflict_seq(
        &self,
        prepped: &[(i64, i64, String, String, bool)],
        base_seq: i64,
        overlap_hi: i64,
    ) -> Result<Option<i64>, String> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT seq, event_time_us, payload FROM session_entries \
                 WHERE workspace_id = ?1 AND session_id = ?2 \
                 AND seq >= ?3 AND seq <= ?4 \
                 ORDER BY seq ASC, event_time_us ASC",
                (
                    self.workspace_id.as_str(),
                    self.session_id.as_str(),
                    base_seq,
                    overlap_hi,
                ),
            )
            .await
            .map_err(|e| format!("cannot read back seq range for concurrent-write check: {e}"))?;

        // Group by seq, keeping only the row(s) with the latest event_time.
        let mut per_seq: std::collections::HashMap<i64, (chrono::NaiveDateTime, Vec<String>)> =
            std::collections::HashMap::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| format!("cannot read back seq range for concurrent-write check: {e}"))?
        {
            let seq = row
                .get_value(0)
                .map_err(|e| format!("cannot read back seq range for concurrent-write check: {e}"))?
                .as_integer()
                .copied()
                .ok_or_else(|| "cannot read back seq range: seq is not an integer".to_string())?;
            let et = row
                .get_value(1)
                .map_err(|e| format!("cannot read back seq range for concurrent-write check: {e}"))?
                .as_integer()
                .copied()
                .ok_or_else(|| {
                    "cannot read back seq range: event_time is not an integer".to_string()
                })?;
            let payload = row
                .get_value(2)
                .map_err(|e| format!("cannot read back seq range for concurrent-write check: {e}"))?
                .as_text()
                .cloned()
                .ok_or_else(|| "cannot read back seq range: payload is not text".to_string())?;
            let et = us_to_datetime(et);
            let entry = per_seq.entry(seq).or_insert((et, Vec::new()));
            match et.cmp(&entry.0) {
                std::cmp::Ordering::Greater => {
                    entry.0 = et;
                    entry.1 = vec![payload];
                }
                std::cmp::Ordering::Equal => entry.1.push(payload),
                std::cmp::Ordering::Less => {}
            }
        }

        // Missing seqs in the window → a foreign writer owns (parts of) it.
        let window_len = (overlap_hi - base_seq + 1) as usize;
        if per_seq.len() < window_len {
            for seq in base_seq..=overlap_hi {
                if !per_seq.contains_key(&seq) {
                    return Ok(Some(seq));
                }
            }
        }

        for (seq, (_, payloads)) in per_seq {
            let idx = (seq - base_seq) as usize;
            let want = &prepped[idx].3;
            if payloads.iter().any(|p| p != want) {
                return Ok(Some(seq));
            }
        }
        Ok(None)
    }

    // ------------------------------------------------------------------
    // sessions — metadata audit table
    // ------------------------------------------------------------------
    //
    // SQLite supports UPDATE, but the sessions table deliberately mirrors
    // Greptime's append-only lifecycle audit log: every create/touch
    // appends a COMPLETE snapshot row (created_at/model/role/parent/
    // entry_count/title/pinned/archived/writer all carried on every row),
    // and the list view deduplicates per session taking the latest
    // last_active_at. There is no partial-row rewrite, so a touch can
    // never wipe immutable columns.

    /// Latest metadata snapshot for one session, or `None` when the
    /// session has no row yet (brand-new session, or a subagent whose
    /// parent has not written its row yet).
    async fn load_meta_row(&self, session_id: &str) -> Result<Option<SessionMeta>, String> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT created_at, last_active_at, model, \"role\", entry_count, \
                        parent_session_id, parent_task_id, title, pinned, archived, writer \
                 FROM sessions \
                 WHERE workspace_id = ?1 AND session_id = ?2 \
                 ORDER BY last_active_at DESC LIMIT 1",
                (self.workspace_id.as_str(), session_id),
            )
            .await
            .map_err(|e| format!("cannot query session metadata: {e}"))?;
        match rows
            .next()
            .await
            .map_err(|e| format!("cannot query session metadata: {e}"))?
        {
            Some(row) => Ok(Some(row_to_meta(&row, session_id)?)),
            None => Ok(None),
        }
    }

    async fn insert_meta(&self, meta: &mut SessionMeta) -> Result<(), String> {
        // Stamp the writer identity at insert time, not at construction
        // (P3): every snapshot row records the process that actually wrote
        // it, and a cross-process resume never replays a stale identity
        // from a row cached by another process. Callers construct with
        // `writer: None`; the stamped value is what lands in the column.
        meta.writer = Some(process_identity().to_owned());
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO sessions \
             (workspace_id, session_id, created_at, last_active_at, model, \"role\", \
              entry_count, parent_session_id, parent_task_id, title, pinned, archived, writer) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            (
                self.workspace_id.as_str(),
                meta.session_id.as_str(),
                datetime_to_us(meta.created_at),
                datetime_to_us(meta.last_active_at),
                meta.model.as_deref(),
                meta.role.as_deref(),
                meta.entry_count,
                meta.parent_session_id.as_deref(),
                meta.parent_task_id,
                meta.title.as_deref(),
                meta.pinned.map(|p| if p { 1 } else { 0 }),
                meta.archived.map(|a| if a { 1 } else { 0 }),
                meta.writer.as_deref(),
            ),
        )
        .await
        .map_err(|e| format!("cannot insert session metadata: {e}"))?;
        Ok(())
    }

    /// The full snapshot the next touch must carry: the cached row when
    /// present, otherwise a read-on-miss of the table (a subagent's first
    /// touch reads back the row its parent wrote at spawn time — R3).
    /// Absence is never cached: a parent may create the row between
    /// touches, and the next touch must see it.
    async fn effective_meta(&self) -> Result<Option<SessionMeta>, String> {
        if let Some(cached) = self.cached_meta.lock().unwrap().clone() {
            return Ok(Some(cached));
        }
        match self.load_meta_row(&self.session_id).await? {
            Some(meta) => {
                *self.cached_meta.lock().unwrap() = Some(meta.clone());
                Ok(Some(meta))
            }
            None => Ok(None),
        }
    }

    /// Snapshot that backfills a parent link onto a session whose first
    /// meta row was written by `SessionFactory::build` (parent = None).
    /// The append-only sessions table has no UPDATE, so the link lands as
    /// a fresh audit row with a newer `last_active_at` (the list view
    /// takes max `last_active_at` per session and therefore picks it up);
    /// immutable columns are carried over from the existing snapshot, and
    /// caller-supplied `model`/`role` win over the existing values.
    /// `parent_session_id`/`parent_task_id` are `Option` so a model-only
    /// backfill (pre-table rows written by [`Self::backfill_sessions`]
    /// have `model = NULL`) leaves the existing links untouched.
    /// Pure — unit-testable without a live DB.
    fn backfill_meta_snapshot(
        existing: &SessionMeta,
        now: chrono::NaiveDateTime,
        model: Option<&str>,
        role: Option<&str>,
        parent_session_id: Option<&str>,
        parent_task_id: Option<i64>,
    ) -> SessionMeta {
        SessionMeta {
            session_id: existing.session_id.clone(),
            created_at: existing.created_at,
            last_active_at: now,
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
            writer: None, // stamped by insert_meta with the writing process
            label: None,  // label lives in running_tasks, resolved at list time
        }
    }

    /// Create the session's first metadata snapshot row. Synchronous
    /// (awaited by the caller) so the list sees the session immediately.
    ///
    /// Idempotent per session: when a row already exists this is a resume,
    /// not a creation, so nothing is appended (a second creation snapshot
    /// would rewrite `created_at` and pollute the audit log). The one
    /// exception is the metadata backfill: a btw/subagent session's first
    /// row can be written without the parent link (unknown at build time)
    /// and/or without a model (pre-table rows from
    /// [`Self::backfill_sessions`] carry `model = NULL`); the caller's
    /// later `create_meta` — the parent process at spawn time, or
    /// `SessionFactory::build` on resume — records what is missing via
    /// [`Self::backfill_meta_snapshot`] — one fresh snapshot, appended
    /// once (a second call finds the field already set and appends
    /// nothing).
    ///
    /// `model` / `role` / parent links are supplied by the caller — the
    /// parent process writes subagent rows at spawn time; the main
    /// session's row is written by `SessionFactory::build`. `title`
    /// names the session at CREATION time only (delegate / btw subagents
    /// pass their task-panel label; main sessions pass `None`): on the
    /// existing-row backfill path an existing title is preserved, so a
    /// resume never rewrites a title.
    pub async fn create_meta(
        &self,
        session_id: &str,
        model: Option<&str>,
        role: Option<&str>,
        parent_session_id: Option<&str>,
        parent_task_id: Option<i64>,
        title: Option<&str>,
    ) -> Result<(), String> {
        if let Some(existing) = self.load_meta_row(session_id).await? {
            // Row already exists (resume, or a btw/subagent row whose
            // parent link was unknown at build time): only ever backfill
            // what is MISSING — the parent link and/or a model — never
            // UPDATE/DELETE, never overwrite an existing link or model,
            // and never append without something to record.
            let backfill_parent =
                parent_session_id.is_some() && existing.parent_session_id.is_none();
            let backfill_model = model.is_some() && existing.model.is_none();
            if backfill_parent || backfill_model {
                let mut meta = Self::backfill_meta_snapshot(
                    &existing,
                    us_to_datetime(next_event_time_us()),
                    model,
                    role,
                    parent_session_id,
                    parent_task_id,
                );
                self.insert_meta(&mut meta).await?;
                *self.cached_meta.lock().unwrap() = Some(meta);
            }
            return Ok(());
        }
        let now = us_to_datetime(next_event_time_us());
        let mut meta = SessionMeta {
            session_id: session_id.to_owned(),
            created_at: now,
            last_active_at: now,
            model: model.map(str::to_owned),
            role: role.map(str::to_owned),
            entry_count: *self.next_seq.lock().unwrap(),
            parent_session_id: parent_session_id.map(str::to_owned),
            parent_task_id,
            title: title.map(str::to_owned), // a fresh session may be named at creation (subagent label)
            pinned: None,                    // a fresh session is unpinned until the user pins it
            archived: None, // a fresh session is unarchived until the user archives it
            writer: None,   // stamped by insert_meta with the writing process
            label: None,    // label lives in running_tasks, resolved at list time
        };
        self.insert_meta(&mut meta).await?;
        *self.cached_meta.lock().unwrap() = Some(meta);
        Ok(())
    }

    /// Append one activity snapshot for the bound session: a full row with
    /// a fresh `last_active_at` and `entry_count = next_seq` — the next
    /// sequence number from connect/append (R2), never a physical row
    /// count, which same-seq retries would overcount.
    ///
    /// Never self-creates (R3): when no row exists yet (no cache and no
    /// row in the table — e.g. a subagent whose parent has not written its
    /// row), the touch is skipped so a subagent can never fabricate its
    /// own row. Callers run this fire-and-forget at turn boundaries (R4);
    /// losing the final touch at process exit is acceptable.
    pub async fn touch_meta(&self) -> Result<(), String> {
        let Some(meta) = self.effective_meta().await? else {
            return Ok(());
        };
        let mut meta = meta;
        meta.last_active_at = us_to_datetime(next_event_time_us());
        meta.entry_count = *self.next_seq.lock().unwrap();
        self.insert_meta(&mut meta).await
    }

    /// Manually name a session: append one full snapshot row carrying the
    /// new `title` and a fresh `last_active_at` (the audit log keeps the
    /// previous title in earlier rows; the list view shows the newest).
    /// `title = None` clears the name (stored as NULL = unnamed).
    ///
    /// Never self-creates (R3, mirroring `touch_meta`): when the session
    /// has no metadata row yet, returns `Ok` and writes nothing. In
    /// practice the server's lazily built sessions always have a row
    /// (`build` → `create_meta`); the no-op guard only protects callers
    /// against racing a session that was never created.
    ///
    /// `session_id` is explicit (like [`Self::delete_meta`]) so a
    /// workspace-scoped meta store can rename historical sessions it is
    /// not bound to; for the bound session the cached snapshot is used
    /// (and refreshed) so the rename is immediately visible to later
    /// touches.
    pub async fn set_title(&self, session_id: &str, title: Option<&str>) -> Result<(), String> {
        let meta = if session_id == self.session_id {
            self.effective_meta().await?
        } else {
            self.load_meta_row(session_id).await?
        };
        let Some(mut meta) = meta else {
            return Ok(());
        };
        meta.title = title.map(str::to_owned);
        meta.last_active_at = us_to_datetime(next_event_time_us());
        self.insert_meta(&mut meta).await?;
        if session_id == self.session_id {
            *self.cached_meta.lock().unwrap() = Some(meta);
        }
        Ok(())
    }

    /// Pin or unpin a session: append one full snapshot row carrying the
    /// new `pinned` flag and a fresh `last_active_at` (the audit log keeps
    /// the previous state in earlier rows; the list view shows the newest
    /// and sorts pinned sessions first). `pinned = false` stores
    /// `Some(false)` — explicitly unpinned, distinct from the `None` of a
    /// never-touched session (both read as unpinned).
    ///
    /// Never self-creates (R3, mirroring `set_title` / `touch_meta`):
    /// when the session has no metadata row yet, returns `Ok` and writes
    /// nothing.
    ///
    /// `session_id` is explicit (like [`Self::set_title`]) so a
    /// workspace-scoped meta store can pin/unpin historical sessions it is
    /// not bound to; for the bound session the cached snapshot is used
    /// (and refreshed) so the change is immediately visible to later
    /// touches.
    pub async fn set_pinned(&self, session_id: &str, pinned: bool) -> Result<(), String> {
        let meta = if session_id == self.session_id {
            self.effective_meta().await?
        } else {
            self.load_meta_row(session_id).await?
        };
        let Some(mut meta) = meta else {
            return Ok(());
        };
        meta.pinned = Some(pinned);
        meta.last_active_at = us_to_datetime(next_event_time_us());
        self.insert_meta(&mut meta).await?;
        if session_id == self.session_id {
            *self.cached_meta.lock().unwrap() = Some(meta);
        }
        Ok(())
    }

    /// Archive or restore a session: append one full snapshot row carrying
    /// the new `archived` flag and a fresh `last_active_at` (the audit log
    /// keeps the previous state in earlier rows; the list view shows the
    /// newest and sorts unarchived sessions before archived ones).
    /// `archived = false` stores `Some(false)` — explicitly restored,
    /// distinct from the `None` of a never-touched session (both read as
    /// unarchived).
    ///
    /// Never self-creates (R3, mirroring `set_pinned` / `set_title` /
    /// `touch_meta`): when the session has no metadata row yet, returns
    /// `Ok` and writes nothing.
    ///
    /// `session_id` is explicit (like [`Self::set_pinned`]) so a
    /// workspace-scoped meta store can archive/restore historical sessions
    /// it is not bound to; for the bound session the cached snapshot is
    /// used (and refreshed) so the change is immediately visible to later
    /// touches.
    pub async fn set_archived(&self, session_id: &str, archived: bool) -> Result<(), String> {
        let meta = if session_id == self.session_id {
            self.effective_meta().await?
        } else {
            self.load_meta_row(session_id).await?
        };
        let Some(mut meta) = meta else {
            return Ok(());
        };
        meta.archived = Some(archived);
        meta.last_active_at = us_to_datetime(next_event_time_us());
        self.insert_meta(&mut meta).await?;
        if session_id == self.session_id {
            *self.cached_meta.lock().unwrap() = Some(meta);
        }
        Ok(())
    }

    /// Latest metadata snapshot per session, newest activity first.
    ///
    /// The table is append-only, so one session has many rows; the list
    /// deduplicates by primary key keeping the row with the maximum
    /// `last_active_at` per session (explicit GROUP BY + JOIN — correct
    /// whether or not the engine auto-dedups same-PK rows at read time).
    pub async fn list_meta(&self) -> Result<Vec<SessionMeta>, String> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT s.session_id, s.created_at, s.last_active_at, s.model, s.\"role\", \
                        s.entry_count, s.parent_session_id, s.parent_task_id, s.title, s.pinned, \
                        s.archived, s.writer \
                 FROM sessions s \
                 INNER JOIN ( \
                     SELECT session_id, MAX(last_active_at) AS max_ts \
                     FROM sessions WHERE workspace_id = ?1 GROUP BY session_id \
                 ) latest \
                   ON latest.session_id = s.session_id AND latest.max_ts = s.last_active_at \
                 WHERE s.workspace_id = ?1 \
                 ORDER BY s.last_active_at DESC",
                (self.workspace_id.as_str(),),
            )
            .await
            .map_err(|e| format!("cannot list session metadata: {e}"))?;
        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| format!("cannot list session metadata: {e}"))?
        {
            let session_id = row
                .get_value(0)
                .map_err(|e| format!("cannot list session metadata: {e}"))?
                .as_text()
                .cloned()
                .ok_or_else(|| {
                    "cannot list session metadata: session_id is not text".to_string()
                })?;
            out.push(meta_from_row_values(&row, &session_id)?);
        }
        Ok(out)
    }

    /// The full lifecycle trace of one session: every snapshot row, oldest
    /// activity first. This is the audit view — [`Self::list_meta`] shows
    /// only the latest snapshot per session.
    pub async fn audit_meta(&self, session_id: &str) -> Result<Vec<SessionMeta>, String> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT created_at, last_active_at, model, \"role\", entry_count, \
                        parent_session_id, parent_task_id, title, pinned, archived, writer \
                 FROM sessions \
                 WHERE workspace_id = ?1 AND session_id = ?2 \
                 ORDER BY last_active_at ASC",
                (self.workspace_id.as_str(), session_id),
            )
            .await
            .map_err(|e| format!("cannot load session metadata audit trail: {e}"))?;
        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| format!("cannot load session metadata audit trail: {e}"))?
        {
            out.push(row_to_meta(&row, session_id)?);
        }
        Ok(out)
    }

    /// Hide a session from the sessions list: delete ALL of its metadata
    /// rows (the complete audit trail). Resume still works because
    /// `session_entries` is untouched. Known limitation (documented, per
    /// the audit-log design without tombstones): a later
    /// [`Self::backfill_sessions`] bootstrap run re-creates the row from
    /// the transcript, so hiding is scoped to the current server lifetime.
    pub async fn delete_meta(&self, session_id: &str) -> Result<(), String> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM sessions WHERE workspace_id = ?1 AND session_id = ?2",
            (self.workspace_id.as_str(), session_id),
        )
        .await
        .map_err(|e| format!("cannot delete session metadata: {e}"))?;
        if session_id == self.session_id {
            *self.cached_meta.lock().unwrap() = None;
        }
        Ok(())
    }

    /// One-time bootstrap migration: aggregate `session_entries` per
    /// session — `MAX(seq)+1` as entry_count (never COUNT, same-seq
    /// retries would overcount) and MIN/MAX(event_time) as the lifecycle
    /// bounds — and insert a snapshot row only for sessions that have NO
    /// metadata row yet. Idempotent: a second run finds every session
    /// already has a row and inserts nothing, so results are identical.
    /// Called once by the server bootstrap; never from `connect` (L3) and
    /// never gated on table emptiness (M1).
    pub async fn backfill_sessions(&self) -> Result<(), String> {
        let (existing, aggregates): (Vec<String>, Vec<BackfillRow>) = {
            let conn = self.conn.lock().await;
            let mut existing_rows = conn
                .query(
                    "SELECT DISTINCT session_id FROM sessions WHERE workspace_id = ?1",
                    (self.workspace_id.as_str(),),
                )
                .await
                .map_err(|e| format!("cannot query existing session metadata: {e}"))?;
            let mut existing: Vec<String> = Vec::new();
            while let Some(row) = existing_rows
                .next()
                .await
                .map_err(|e| format!("cannot query existing session metadata: {e}"))?
            {
                existing.push(
                    row.get_value(0)
                        .map_err(|e| format!("cannot query existing session metadata: {e}"))?
                        .as_text()
                        .cloned()
                        .ok_or_else(|| {
                            "cannot query existing session metadata: session_id is not text"
                                .to_string()
                        })?,
                );
            }
            drop(existing_rows);

            let mut agg_rows = conn
                .query(
                    "SELECT session_id, MAX(seq) + 1 AS entry_count, \
                            MIN(event_time_us) AS created_at, MAX(event_time_us) AS last_active_at \
                     FROM session_entries \
                     WHERE workspace_id = ?1 \
                     GROUP BY session_id",
                    (self.workspace_id.as_str(),),
                )
                .await
                .map_err(|e| {
                    format!("cannot aggregate session_entries for metadata backfill: {e}")
                })?;
            let mut aggregates: Vec<BackfillRow> = Vec::new();
            while let Some(row) = agg_rows.next().await.map_err(|e| {
                format!("cannot aggregate session_entries for metadata backfill: {e}")
            })? {
                let session_id = row
                    .get_value(0)
                    .map_err(|e| {
                        format!("cannot aggregate session_entries for metadata backfill: {e}")
                    })?
                    .as_text()
                    .cloned()
                    .ok_or_else(|| {
                        "cannot aggregate session_entries: session_id is not text".to_string()
                    })?;
                let entry_count = row
                    .get_value(1)
                    .map_err(|e| {
                        format!("cannot aggregate session_entries for metadata backfill: {e}")
                    })?
                    .as_integer()
                    .copied()
                    .ok_or_else(|| {
                        "cannot aggregate session_entries: entry_count is not an integer"
                            .to_string()
                    })?;
                let created_us = row
                    .get_value(2)
                    .map_err(|e| {
                        format!("cannot aggregate session_entries for metadata backfill: {e}")
                    })?
                    .as_integer()
                    .copied()
                    .ok_or_else(|| {
                        "cannot aggregate session_entries: created_at is not an integer".to_string()
                    })?;
                let last_us = row
                    .get_value(3)
                    .map_err(|e| {
                        format!("cannot aggregate session_entries for metadata backfill: {e}")
                    })?
                    .as_integer()
                    .copied()
                    .ok_or_else(|| {
                        "cannot aggregate session_entries: last_active_at is not an integer"
                            .to_string()
                    })?;
                aggregates.push((session_id, entry_count, created_us, last_us));
            }
            (existing, aggregates)
        };

        for (session_id, entry_count, created_us, last_us) in aggregates {
            if existing.iter().any(|id| id == &session_id) {
                continue;
            }
            self.insert_meta(&mut SessionMeta {
                session_id,
                created_at: us_to_datetime(created_us),
                last_active_at: us_to_datetime(last_us),
                model: None,
                role: None,
                entry_count,
                parent_session_id: None,
                parent_task_id: None,
                title: None,    // pre-table sessions have no user-assigned name
                pinned: None,   // pre-table sessions are unpinned
                archived: None, // pre-table sessions are unarchived
                writer: None,   // stamped by insert_meta with the writing process
                label: None,    // label lives in running_tasks, resolved at list time
            })
            .await?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // running_tasks — background-task state table
    // ------------------------------------------------------------------
    //
    // These are the SQLite counterpart of the JSONL
    // `*.background.jsonl` record files (`session::Session`). Unlike the
    // transcript, they are keyed by the *recording* session (the one whose
    // task list owns the row) and may be written by a store bound to a
    // different transcript session (a delegate row lives under the parent
    // session, with the subagent session id in `subagent_session_id`).
    // That is why every method takes the session id explicitly instead of
    // reusing the bound `self.session_id`.

    /// Record a freshly started background task so a later launch can tell
    /// the user what died with the previous process. Last-write-wins per
    /// (workspace, session, task_id): re-recording an existing key (the
    /// per-process task counter restarts across processes) overwrites the
    /// row instead of erroring on the primary-key conflict, matching
    /// Greptime's restart-time semantics.
    pub async fn record_task_start(
        &self,
        session_id: &str,
        task_id: u64,
        label: &str,
        subagent_session_id: Option<&str>,
    ) -> Result<(), String> {
        let started_at = next_event_time_us();
        let task_id =
            i64::try_from(task_id).map_err(|_| "background task id does not fit in BIGINT")?;
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO running_tasks \
             (workspace_id, session_id, task_id, label, subagent_session_id, started_at_us, owner_identity) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT (workspace_id, session_id, task_id) DO UPDATE SET \
                 label = excluded.label, \
                 subagent_session_id = excluded.subagent_session_id, \
                 started_at_us = excluded.started_at_us, \
                 owner_identity = excluded.owner_identity",
            (
                self.workspace_id.as_str(),
                session_id,
                task_id,
                label,
                subagent_session_id,
                started_at,
                crate::session_store::process_identity(),
            ),
        )
        .await
        .map_err(|e| format!("cannot record background task start: {e}"))?;
        Ok(())
    }

    /// Forget one task: its completion arrived while the process was alive.
    pub async fn clear_task(&self, session_id: &str, task_id: u64) -> Result<(), String> {
        let task_id =
            i64::try_from(task_id).map_err(|_| "background task id does not fit in BIGINT")?;
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM running_tasks \
             WHERE workspace_id = ?1 AND session_id = ?2 AND task_id = ?3",
            (self.workspace_id.as_str(), session_id, task_id),
        )
        .await
        .map_err(|e| format!("cannot clear background task: {e}"))?;
        Ok(())
    }

    /// Tasks recorded by a previous process that died before their
    /// completion arrived. Consumes (deletes) all rows for the session and
    /// returns the labels so the caller can inject the "killed on exit"
    /// notice. Rows scoped to another session are untouched.
    pub async fn take_unfinished_tasks(&self, session_id: &str) -> Result<Vec<String>, String> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT task_id, label, subagent_session_id FROM running_tasks \
                 WHERE workspace_id = ?1 AND session_id = ?2",
                (self.workspace_id.as_str(), session_id),
            )
            .await
            .map_err(|e| format!("cannot load unfinished background tasks: {e}"))?;
        let mut labels = Vec::new();
        let mut non_empty = false;
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| format!("cannot load unfinished background tasks: {e}"))?
        {
            non_empty = true;
            let task_id = row
                .get_value(0)
                .map_err(|e| format!("cannot load unfinished background tasks: {e}"))?
                .as_integer()
                .copied()
                .ok_or_else(|| {
                    "cannot load unfinished background tasks: task_id is not an integer".to_string()
                })?;
            let label = row
                .get_value(1)
                .map_err(|e| format!("cannot load unfinished background tasks: {e}"))?
                .as_text()
                .cloned()
                .ok_or_else(|| {
                    "cannot load unfinished background tasks: label is not text".to_string()
                })?;
            let subagent = row
                .get_value(2)
                .map_err(|e| format!("cannot load unfinished background tasks: {e}"))?
                .as_text()
                .cloned();
            labels.push(crate::session::format_unfinished(
                task_id.max(0) as u64,
                &label,
                subagent.as_deref(),
            ));
        }
        if non_empty {
            conn.execute(
                "DELETE FROM running_tasks \
                 WHERE workspace_id = ?1 AND session_id = ?2",
                (self.workspace_id.as_str(), session_id),
            )
            .await
            .map_err(|e| format!("cannot clear unfinished background tasks: {e}"))?;
        }
        Ok(labels)
    }

    /// True when every unfinished-task row for `session_id` was recorded
    /// by a now-dead process (the server-attach probe that decides between
    /// `Consume` — inject the "killed with the process" notice — and
    /// `Preserve` — leave the records for a possibly-live owning process).
    ///
    /// Conservative: any uncertainty reports false — a NULL `owner` (an
    /// old row written before the column shipped) counts as alive, as does
    /// a live owner or an unjudgeable identity (see
    /// [`crate::session_store::owner_alive`]). No rows → true (nothing to
    /// consume; the Consume path is a no-op).
    pub async fn unfinished_owner_all_dead(&self, session_id: &str) -> Result<bool, String> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT owner_identity FROM running_tasks \
                 WHERE workspace_id = ?1 AND session_id = ?2",
                (self.workspace_id.as_str(), session_id),
            )
            .await
            .map_err(|e| format!("cannot load unfinished background task owners: {e}"))?;
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| format!("cannot load unfinished background task owners: {e}"))?
        {
            let owner = row
                .get_value(0)
                .map_err(|e| format!("cannot load unfinished background task owners: {e}"))?
                .as_text()
                .cloned();
            match owner {
                None => return Ok(false), // old row without owner: alive
                Some(owner) => {
                    if crate::session_store::owner_alive(&owner) {
                        return Ok(false); // owner still alive
                    }
                }
            }
        }
        // No rows, or every row's owner is dead.
        Ok(true)
    }

    /// Same as [`Self::take_unfinished_tasks`] but keyed by
    /// `subagent_session_id`: the rows a killed parent left for one of its
    /// background delegate subagents. The table is global (unlike JSONL
    /// per-session files), so a resumed subagent can look up its own
    /// leftovers from any parent session. The subagent session id is
    /// implied by the lookup, so labels carry no `(session: …)` suffix.
    pub async fn take_unfinished_tasks_for_subagent(
        &self,
        subagent_session_id: &str,
    ) -> Result<Vec<String>, String> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT task_id, label FROM running_tasks \
                 WHERE workspace_id = ?1 AND subagent_session_id = ?2",
                (self.workspace_id.as_str(), subagent_session_id),
            )
            .await
            .map_err(|e| format!("cannot load unfinished subagent tasks: {e}"))?;
        let mut labels = Vec::new();
        let mut non_empty = false;
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| format!("cannot load unfinished subagent tasks: {e}"))?
        {
            non_empty = true;
            let task_id = row
                .get_value(0)
                .map_err(|e| format!("cannot load unfinished subagent tasks: {e}"))?
                .as_integer()
                .copied()
                .ok_or_else(|| {
                    "cannot load unfinished subagent tasks: task_id is not an integer".to_string()
                })?;
            let label = row
                .get_value(1)
                .map_err(|e| format!("cannot load unfinished subagent tasks: {e}"))?
                .as_text()
                .cloned()
                .ok_or_else(|| {
                    "cannot load unfinished subagent tasks: label is not text".to_string()
                })?;
            labels.push(crate::session::format_unfinished(
                task_id.max(0) as u64,
                &label,
                None,
            ));
        }
        if non_empty {
            conn.execute(
                "DELETE FROM running_tasks \
                 WHERE workspace_id = ?1 AND subagent_session_id = ?2",
                (self.workspace_id.as_str(), subagent_session_id),
            )
            .await
            .map_err(|e| format!("cannot clear unfinished subagent tasks: {e}"))?;
        }
        Ok(labels)
    }

    /// The task-panel label for a subagent session: the label of the newest
    /// `running_tasks` row whose `subagent_session_id` matches. A subagent
    /// can have several rows (one per delegate task, possibly from
    /// different parents); the most recently started survives. Rows are
    /// consumed when the task completes, so this returns `None` for a
    /// subagent with no live delegate task. Non-destructive (unlike the
    /// `take_unfinished_*` lookups) — called from `/api/sessions` listing.
    pub async fn label_for_subagent(
        &self,
        subagent_session_id: &str,
    ) -> Result<Option<String>, String> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT label FROM running_tasks \
                 WHERE workspace_id = ?1 AND subagent_session_id = ?2 \
                 ORDER BY started_at_us DESC LIMIT 1",
                (self.workspace_id.as_str(), subagent_session_id),
            )
            .await
            .map_err(|e| format!("cannot look up subagent task label: {e}"))?;
        match rows
            .next()
            .await
            .map_err(|e| format!("cannot look up subagent task label: {e}"))?
        {
            Some(row) => row
                .get_value(0)
                .map_err(|e| format!("cannot look up subagent task label: {e}"))?
                .as_text()
                .cloned()
                .map(Some)
                .ok_or_else(|| "cannot look up subagent task label: not text".to_string()),
            None => Ok(None),
        }
    }

    /// The batched form of [`Self::label_for_subagent`], used by the
    /// sessions list to resolve every subagent label in ONE query instead
    /// of a per-subagent N+1. Returns the full
    /// `subagent_session_id → label` map in a single scan. A subagent can
    /// have several rows (one per delegate task, possibly from different
    /// parents); rows are inserted oldest→newest so each later insert
    /// overwrites the previous one and `ORDER BY started_at_us ASC` makes
    /// the newest label the final (winning) value — the same "newest
    /// wins" rule as the per-session lookup. Non-destructive (unlike the
    /// `take_unfinished_*` lookups) — called from `/api/sessions`
    /// listing. A subagent with no live delegate task is simply absent
    /// from the map (the caller reads it as `None`).
    pub async fn all_subagent_labels(&self) -> Result<HashMap<String, Option<String>>, String> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT subagent_session_id, label FROM running_tasks \
                 WHERE workspace_id = ?1 AND subagent_session_id IS NOT NULL \
                 ORDER BY started_at_us ASC",
                (self.workspace_id.as_str(),),
            )
            .await
            .map_err(|e| format!("cannot load all subagent task labels: {e}"))?;
        let mut labels = HashMap::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| format!("cannot load all subagent task labels: {e}"))?
        {
            let Some(subagent_session_id) = row
                .get_value(0)
                .map_err(|e| format!("cannot load all subagent task labels: {e}"))?
                .as_text()
                .cloned()
            else {
                // NULL subagent_session_id is excluded by the WHERE clause;
                // skip defensively instead of failing the whole batch.
                continue;
            };
            let label = row
                .get_value(1)
                .map_err(|e| format!("cannot load all subagent task labels: {e}"))?
                .as_text()
                .cloned();
            labels.insert(subagent_session_id, label);
        }
        Ok(labels)
    }

    /// Rewrite the entire session log (used for legacy migration). A no-op
    /// for this backend, exactly like Greptime: the session log is
    /// append-only (the enum arm decides append-vs-rewrite; SQLite uses
    /// append).
    pub async fn rewrite(&self, _entries: &[SessionEntry]) -> Result<(), String> {
        Ok(())
    }
}

/// Map a `sessions` table row (all columns except `session_id`, which is
/// passed in) to a [`SessionMeta`]. Column layout of the shared SELECT:
/// `created_at, last_active_at, model, "role", entry_count,
/// parent_session_id, parent_task_id, title, pinned, archived, writer`.
fn row_to_meta(row: &turso::Row, session_id: &str) -> Result<SessionMeta, String> {
    let created_at = us_to_datetime(
        row.get_value(0)
            .map_err(|e| format!("cannot query session metadata: {e}"))?
            .as_integer()
            .copied()
            .ok_or_else(|| {
                "cannot query session metadata: created_at is not an integer".to_string()
            })?,
    );
    let last_active_at = us_to_datetime(
        row.get_value(1)
            .map_err(|e| format!("cannot query session metadata: {e}"))?
            .as_integer()
            .copied()
            .ok_or_else(|| {
                "cannot query session metadata: last_active_at is not an integer".to_string()
            })?,
    );
    let entry_count = row
        .get_value(4)
        .map_err(|e| format!("cannot query session metadata: {e}"))?
        .as_integer()
        .copied()
        .ok_or_else(|| {
            "cannot query session metadata: entry_count is not an integer".to_string()
        })?;
    let parent_task_id = row
        .get_value(6)
        .map_err(|e| format!("cannot query session metadata: {e}"))?
        .as_integer()
        .copied();
    let title = row
        .get_value(7)
        .map_err(|e| format!("cannot query session metadata: {e}"))?
        .as_text()
        .cloned();
    let pinned = match row
        .get_value(8)
        .map_err(|e| format!("cannot query session metadata: {e}"))?
        .as_integer()
        .copied()
    {
        Some(1) => Some(true),
        Some(0) => Some(false),
        Some(_) => {
            return Err("cannot query session metadata: pinned is not 0 or 1".to_string());
        }
        None => None,
    };
    let archived = match row
        .get_value(9)
        .map_err(|e| format!("cannot query session metadata: {e}"))?
        .as_integer()
        .copied()
    {
        Some(1) => Some(true),
        Some(0) => Some(false),
        Some(_) => {
            return Err("cannot query session metadata: archived is not 0 or 1".to_string());
        }
        None => None,
    };
    Ok(SessionMeta {
        session_id: session_id.to_owned(),
        created_at,
        last_active_at,
        model: row
            .get_value(2)
            .map_err(|e| format!("cannot query session metadata: {e}"))?
            .as_text()
            .cloned(),
        role: row
            .get_value(3)
            .map_err(|e| format!("cannot query session metadata: {e}"))?
            .as_text()
            .cloned(),
        entry_count,
        parent_session_id: row
            .get_value(5)
            .map_err(|e| format!("cannot query session metadata: {e}"))?
            .as_text()
            .cloned(),
        parent_task_id,
        title,
        pinned,
        archived,
        writer: row
            .get_value(10)
            .map_err(|e| format!("cannot query session metadata: {e}"))?
            .as_text()
            .cloned(),
        label: None, // label lives in running_tasks, resolved at list time
    })
}

/// Map a `list_meta` row (session_id already extracted as column 0) to a
/// [`SessionMeta`]. Column layout of the list SELECT: `session_id,
/// created_at, last_active_at, model, "role", entry_count,
/// parent_session_id, parent_task_id, title, pinned, archived, writer`.
fn meta_from_row_values(row: &turso::Row, session_id: &str) -> Result<SessionMeta, String> {
    let created_at = us_to_datetime(
        row.get_value(1)
            .map_err(|e| format!("cannot list session metadata: {e}"))?
            .as_integer()
            .copied()
            .ok_or_else(|| {
                "cannot list session metadata: created_at is not an integer".to_string()
            })?,
    );
    let last_active_at = us_to_datetime(
        row.get_value(2)
            .map_err(|e| format!("cannot list session metadata: {e}"))?
            .as_integer()
            .copied()
            .ok_or_else(|| {
                "cannot list session metadata: last_active_at is not an integer".to_string()
            })?,
    );
    let entry_count = row
        .get_value(5)
        .map_err(|e| format!("cannot list session metadata: {e}"))?
        .as_integer()
        .copied()
        .ok_or_else(|| "cannot list session metadata: entry_count is not an integer".to_string())?;
    let parent_task_id = row
        .get_value(7)
        .map_err(|e| format!("cannot list session metadata: {e}"))?
        .as_integer()
        .copied();
    let title = row
        .get_value(8)
        .map_err(|e| format!("cannot list session metadata: {e}"))?
        .as_text()
        .cloned();
    let pinned = match row
        .get_value(9)
        .map_err(|e| format!("cannot list session metadata: {e}"))?
        .as_integer()
        .copied()
    {
        Some(1) => Some(true),
        Some(0) => Some(false),
        Some(_) => {
            return Err("cannot list session metadata: pinned is not 0 or 1".to_string());
        }
        None => None,
    };
    let archived = match row
        .get_value(10)
        .map_err(|e| format!("cannot list session metadata: {e}"))?
        .as_integer()
        .copied()
    {
        Some(1) => Some(true),
        Some(0) => Some(false),
        Some(_) => {
            return Err("cannot list session metadata: archived is not 0 or 1".to_string());
        }
        None => None,
    };
    Ok(SessionMeta {
        session_id: session_id.to_owned(),
        created_at,
        last_active_at,
        model: row
            .get_value(3)
            .map_err(|e| format!("cannot list session metadata: {e}"))?
            .as_text()
            .cloned(),
        role: row
            .get_value(4)
            .map_err(|e| format!("cannot list session metadata: {e}"))?
            .as_text()
            .cloned(),
        entry_count,
        parent_session_id: row
            .get_value(6)
            .map_err(|e| format!("cannot list session metadata: {e}"))?
            .as_text()
            .cloned(),
        parent_task_id,
        title,
        pinned,
        archived,
        writer: row
            .get_value(11)
            .map_err(|e| format!("cannot list session metadata: {e}"))?
            .as_text()
            .cloned(),
        label: None, // label lives in running_tasks, resolved at list time
    })
}

/// Map raw turso rows (seq, event_time_us, payload) to the dedup input
/// tuples. Consumes the `Rows` iterator.
async fn rows_to_raw(
    mut rows: turso::Rows,
) -> Result<Vec<(i64, chrono::NaiveDateTime, String)>, String> {
    let mut raw = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| format!("cannot load session entries: {e}"))?
    {
        let seq = row
            .get_value(0)
            .map_err(|e| format!("cannot load session entries: {e}"))?
            .as_integer()
            .copied()
            .ok_or_else(|| "cannot load session entries: seq is not an integer".to_string())?;
        let et_us = row
            .get_value(1)
            .map_err(|e| format!("cannot load session entries: {e}"))?
            .as_integer()
            .copied()
            .ok_or_else(|| {
                "cannot load session entries: event_time is not an integer".to_string()
            })?;
        let payload = row
            .get_value(2)
            .map_err(|e| format!("cannot load session entries: {e}"))?
            .as_text()
            .cloned()
            .ok_or_else(|| "cannot load session entries: payload is not text".to_string())?;
        raw.push((seq, us_to_datetime(et_us), payload));
    }
    Ok(raw)
}

/// Build a multi-row INSERT with 7 bound parameters per row (workspace_id,
/// session_id, seq, event_time_us, entry_kind, payload, is_error);
/// schema_version is hardcoded as 1.
/// Example (1 row):
/// `INSERT INTO session_entries (workspace_id, session_id, seq, event_time_us,
///  entry_kind, payload, schema_version, is_error) VALUES (?1,?2,?3,?4,?5,?6,1,?7)`
fn build_multi_row_insert(row_count: usize) -> String {
    let mut sql = String::with_capacity(200 + row_count * 50);
    sql.push_str(
        "INSERT INTO session_entries \
         (workspace_id, session_id, seq, event_time_us, entry_kind, payload, \
          schema_version, is_error) VALUES ",
    );
    for i in 0..row_count {
        if i > 0 {
            sql.push(',');
        }
        let b = i * 7;
        sql.push_str(&format!(
            "(?{b1},?{b2},?{b3},?{b4},?{b5},?{b6},1,?{b7})",
            b1 = b + 1,
            b2 = b + 2,
            b3 = b + 3,
            b4 = b + 4,
            b5 = b + 5,
            b6 = b + 6,
            b7 = b + 7
        ));
    }
    sql
}

#[cfg(test)]
#[path = "session_sqlite_tests.rs"]
mod tests;
