//! GreptimeDB-backed session storage. Optional runtime-selectable
//! backend via `[session] backend = "greptime"`. Still experimental.
//! Uses tokio-postgres for both read and write against the
//! `session_entries` transcript table and the `running_tasks` state table.
//!
//! Non-goals: no Storage trait, no migration of existing JSONL sessions.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use tokio_postgres::NoTls;

use crate::agent::SessionEntry;
use crate::output_receipt::{
    ReceiptError, ReceiptErrorKind, VerifiedRef, field_bytes, validate_location_for_store,
};
use crate::session_store::{
    EntryLocation, LocatedKey, SessionMeta, UsageRow, datetime_to_us, dedup_finished_rows,
    dedup_raw_entries, dedup_raw_located, entry_kind, entry_payload_hash, format_conflict_error,
    is_error, next_event_time_us, process_identity, us_to_datetime, workspace_id_fingerprint,
};
// Public path preserved for `src/bin/import_jsonl.rs` (was a `pub fn`
// defined here before the shared-helper extraction).
pub use crate::session_store::derive_workspace_id;

/// DDL for the session table. Idempotent.
const CREATE_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS session_entries (
    workspace_id STRING NOT NULL,
    session_id STRING NOT NULL,
    seq BIGINT NOT NULL,
    event_time TIMESTAMP(9) NOT NULL TIME INDEX,
    entry_kind STRING NOT NULL,
    payload STRING NOT NULL,
    schema_version INT NOT NULL DEFAULT 1,
    is_error BOOLEAN NOT NULL DEFAULT FALSE,
    appended_at TIMESTAMP(9) NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, session_id)
) WITH (
    append_mode = 'true',
    sst_format = 'flat',
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
/// table, not a log: default (non-append) mode, because GreptimeDB forbids
/// DELETE under `append_mode = 'true'` and last-write-wins per primary key
/// is exactly the semantics a task registry wants (`task_id` is the
/// per-process background counter and may repeat across restarts — the new
/// row simply overwrites the old one).
const CREATE_TABLE_RUNNING_TASKS: &str = r#"
CREATE TABLE IF NOT EXISTS running_tasks (
    workspace_id STRING NOT NULL,
    session_id STRING NOT NULL,
    task_id BIGINT NOT NULL,
    label STRING NOT NULL,
    full_command STRING NULL,
    subagent_session_id STRING NULL,
    started_at TIMESTAMP(9) NOT NULL TIME INDEX,
    owner_identity STRING NULL,
    PRIMARY KEY (workspace_id, session_id, task_id)
) WITH (
    sst_format = 'flat',
)
"#;

/// DDL for the sessions metadata table. Idempotent. NOT `append_mode`
/// (unlike `session_entries`) so `delete_meta` can DELETE rows.
///
/// Semantics: an append-only lifecycle AUDIT log, not a one-row-per-session
/// upsert table. Every create/touch appends a COMPLETE snapshot row
/// (created_at/model/role/parent/entry_count/title/pinned/archived all
/// carried on every row) at a fresh `last_active_at`; the TIME INDEX keeps
/// the rows ordered, and the list view deduplicates per primary key taking
/// the latest `last_active_at` (explicitly in SQL — the query is correct
/// whether or not the engine auto-dedups same-PK rows). Because each row
/// is a full snapshot, a touch can never wipe immutable columns: there is
/// no partial-row rewrite at all.
///
/// `title` is the user-assigned session name (manual, never auto-generated;
/// `NULL` = unnamed, the frontend shows the id). `pinned` is the user pin
/// flag (`NULL` = never touched, reads as unpinned; the list sorts pinned
/// sessions first). `archived` is the user archive flag (`NULL` = never
/// touched, reads as unarchived; archived sessions are hidden from the
/// default list and folded into the sidebar's archived group). All are
/// added to pre-existing tables by the idempotent migration in `connect` —
/// see the `ALTER TABLE` comment there.
///
/// `writer` is the process identity of the process that wrote this snapshot
/// row (the most recent `insert_meta`'s stamping process, `pid@hostname#nonce`
/// — see [`process_identity`]). It serves two purposes: an audit trail of
/// who wrote each snapshot, and a best-effort hint in concurrent-write
/// conflict errors (the "recent snapshot writer", which may not be the
/// conflicting writer — see the conflict bail). Added to pre-existing
/// tables by the same idempotent migration as `title`/`pinned`/`archived`.
const CREATE_TABLE_SESSIONS: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    workspace_id STRING NOT NULL,
    session_id STRING NOT NULL,
    created_at TIMESTAMP(9) NOT NULL,
    last_active_at TIMESTAMP(9) NOT NULL,
    model STRING NULL,
    "role" STRING NULL,
    title STRING NULL,
    pinned BOOLEAN NULL,
    archived BOOLEAN NULL,
    writer STRING NULL,
    entry_count BIGINT NOT NULL DEFAULT 0,
    parent_session_id STRING NULL,
    parent_task_id BIGINT NULL,
    TIME INDEX (last_active_at),
    PRIMARY KEY (workspace_id, session_id)
)
"#;

/// DDL for the token-usage statistics table. Idempotent. One row per model
/// call whose usage is worth accounting (regular turns, compactions, the
/// desktop-pet summarizer), scoped to (workspace, session) like
/// `session_entries`. `append_mode` (same as `session_entries`): the
/// primary key `(workspace_id, session_id, seq)` never repeats in practice
/// and append mode keeps a same-PK retry from silently overwriting. For
/// "regular"/"compact" rows `seq` is the just-committed assistant/compaction
/// entry's actual `session_entries.seq` (threaded from the runner's commit
/// result); "summarizer" rows have no committed entry and fall back to the
/// strictly monotonic per-process microsecond clock ([`next_event_time_us`]).
/// The read path is a plain `GROUP BY session_id,
/// model, kind` aggregate ([`Self::usage_summary`]).
///
/// The per-call enrichment columns (`cache_hit_tokens`/`cache_miss_tokens`/
/// `reasoning_tokens`/`finish_reason`) are nullable and were added after
/// the table shipped (same probe-then-ALTER migration as `sessions.title`
/// etc.): old rows read them back as NULL. `seq` carries the ACTUAL
/// `session_entries.seq` of the assistant/compaction entry the usage row
/// corresponds to (exact equality join for "regular"/"compact" — see
/// [`Self::append_usage`]).
const CREATE_TABLE_USAGE: &str = r#"
CREATE TABLE IF NOT EXISTS usage_entries (
    workspace_id STRING NOT NULL,
    session_id STRING NOT NULL,
    seq BIGINT NOT NULL,
    event_time TIMESTAMP(9) NOT NULL TIME INDEX,
    model STRING NOT NULL,
    kind STRING NOT NULL,
    input_tokens BIGINT NOT NULL,
    output_tokens BIGINT NOT NULL,
    cache_hit_tokens BIGINT NULL,
    cache_miss_tokens BIGINT NULL,
    reasoning_tokens BIGINT NULL,
    finish_reason STRING NULL,
    PRIMARY KEY (workspace_id, session_id, seq)
) WITH (
    append_mode = 'true',
    sst_format = 'flat',
)
"#;

fn advance_cursor(cursor: &AtomicI64, next: i64) {
    cursor.fetch_max(next, Ordering::AcqRel);
}

pub struct GreptimeSession {
    client: tokio_postgres::Client,
    /// Next sequence number for appends within this session.
    next_seq: AtomicI64,
    workspace_id: String,
    session_id: String,
    /// Hex SHA-256 of the backend-instance identity (backend kind + the
    /// connection string). Receipts carry this hash — never the connection
    /// string — and `read_field` rejects a receipt bound to another
    /// GreptimeDB instance before querying.
    backend_fp: String,
    /// The session's latest metadata snapshot, cached at connect time so
    /// every touch carries the immutable columns (created_at/model/role/
    /// parent/title/pinned) without re-reading them. `None` = no row yet
    /// (brand-new session, or a subagent whose parent has not written its
    /// row yet).
    /// Interior mutability because every touch path takes `&self`.
    cached_meta: std::sync::Mutex<Option<SessionMeta>>,
}

impl GreptimeSession {
    async fn connect_client(conn: &str) -> Result<tokio_postgres::Client> {
        let (client, connection) = tokio_postgres::connect(conn, NoTls)
            .await
            .context("cannot connect to GreptimeDB")?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("greptime session connection error: {e}");
            }
        });
        Ok(client)
    }

    /// Connect a workspace/session-bound reader without running any setup.
    /// This is intentionally only a connection and driver spawn: no DDL,
    /// migration, schema probe, backfill, or write is performed.
    pub async fn connect_read_only(
        conn: &str,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<Self> {
        let client = Self::connect_client(conn).await?;
        let backend_fp = crate::session_store::backend_instance_fingerprint("greptime", conn);
        Ok(Self {
            client,
            next_seq: AtomicI64::new(0),
            workspace_id: workspace_id.to_owned(),
            session_id: session_id.to_owned(),
            backend_fp,
            cached_meta: std::sync::Mutex::new(None),
        })
    }

    /// Connect and ensure the table exists. `conn` is a tokio-postgres
    /// connection string, e.g. "host=127.0.0.1 port=4002 dbname=public".
    /// `workspace_id` is derived from the canonical workspace root and
    /// scopes sessions to their workspace.
    pub async fn connect(conn: &str, workspace_id: &str, session_id: &str) -> Result<Self> {
        let client = Self::connect_client(conn).await?;
        client
            .execute(CREATE_TABLE, &[])
            .await
            .context("cannot create session_entries table")?;
        client
            .execute(CREATE_TABLE_RUNNING_TASKS, &[])
            .await
            .context("cannot create running_tasks table")?;
        client
            .execute(CREATE_TABLE_SESSIONS, &[])
            .await
            .context("cannot create sessions table")?;
        client
            .execute(CREATE_TABLE_USAGE, &[])
            .await
            .context("cannot create usage_entries table")?;

        // Idempotent schema migration for the `title`, `pinned`,
        // `archived` and `writer` columns — a table-structure evolution:
        // `sessions` shipped without them, so pre-existing databases need
        // an ALTER (fresh databases already have them via
        // CREATE_TABLE_SESSIONS above). There are no historical rows to
        // backfill: old rows simply read the columns back as NULL (the
        // read path treats them as `Option`). A failed migration must NOT
        // block the connection (GreptimeDB's pg-wire supports
        // `ALTER TABLE ... ADD COLUMN`, but if it errors anyway — e.g. an
        // ancient engine — we keep running with the feature degraded: the
        // meta-row cache below is skipped and later column-bearing queries
        // fail loudly with context; transcript operations are unaffected).
        let mut title_available = true;
        let mut pinned_available = true;
        let mut archived_available = true;
        let mut writer_available = true;
        let columns: Vec<String> = client
            .query(
                "SELECT column_name FROM information_schema.columns \
                 WHERE table_name = 'sessions'",
                &[],
            )
            .await
            .context("cannot inspect sessions table schema")?
            .iter()
            .map(|row| row.get("column_name"))
            .collect();
        if !columns.iter().any(|c| c == "title") {
            match client
                .execute("ALTER TABLE sessions ADD COLUMN title STRING NULL", &[])
                .await
            {
                Ok(_) => {}
                Err(error) => {
                    title_available = false;
                    eprintln!(
                        "e-agent: cannot add sessions.title column (session titles unavailable): \
                         {error:#}"
                    );
                }
            }
        }
        if !columns.iter().any(|c| c == "pinned") {
            match client
                .execute("ALTER TABLE sessions ADD COLUMN pinned BOOLEAN NULL", &[])
                .await
            {
                Ok(_) => {}
                Err(error) => {
                    pinned_available = false;
                    eprintln!(
                        "e-agent: cannot add sessions.pinned column (session pinning unavailable): \
                         {error:#}"
                    );
                }
            }
        }
        if !columns.iter().any(|c| c == "archived") {
            match client
                .execute("ALTER TABLE sessions ADD COLUMN archived BOOLEAN NULL", &[])
                .await
            {
                Ok(_) => {}
                Err(error) => {
                    archived_available = false;
                    eprintln!(
                        "e-agent: cannot add sessions.archived column (session archiving unavailable): \
                         {error:#}"
                    );
                }
            }
        }
        if !columns.iter().any(|c| c == "writer") {
            match client
                .execute("ALTER TABLE sessions ADD COLUMN writer STRING NULL", &[])
                .await
            {
                Ok(_) => {}
                Err(error) => {
                    writer_available = false;
                    eprintln!(
                        "e-agent: cannot add sessions.writer column (writer audit unavailable): \
                         {error:#}"
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
            let task_columns: Vec<String> = client
                .query(
                    "SELECT column_name FROM information_schema.columns \
                     WHERE table_name = 'running_tasks'",
                    &[],
                )
                .await
                .context("cannot inspect running_tasks table schema")?
                .iter()
                .map(|row| row.get("column_name"))
                .collect();
            if !task_columns.iter().any(|c| c == "owner_identity") {
                match client
                    .execute(
                        "ALTER TABLE running_tasks ADD COLUMN owner_identity STRING NULL",
                        &[],
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(error) => {
                        eprintln!(
                            "e-agent: cannot add running_tasks.owner_identity column \
                             (background-task owner liveness unavailable): {error:#}"
                        );
                    }
                }
            }
            // Same probe-then-ALTER for the `full_command` column (the
            // full untruncated bash command, added after the table shipped
            // so the UI can show it after a restart). Pre-existing
            // databases need the ALTER; fresh databases already have the
            // column via CREATE_TABLE_RUNNING_TASKS above. Old rows read
            // the column back as NULL, which `task_full_command` reports
            // as `None`. A failed ALTER does NOT block the connection: the
            // feature degrades — `record_task_start` fails loudly on the
            // missing column, transcript operations are unaffected (same
            // philosophy as the owner_identity migration above).
            if !task_columns.iter().any(|c| c == "full_command") {
                match client
                    .execute(
                        "ALTER TABLE running_tasks ADD COLUMN full_command STRING NULL",
                        &[],
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(error) => {
                        eprintln!(
                            "e-agent: cannot add running_tasks.full_command column \
                             (background-task full-command persistence unavailable): {error:#}"
                        );
                    }
                }
            }
        }

        // Same probe-then-ALTER migration for the per-call usage
        // enrichment columns of `usage_entries` (cache/reasoning/finish
        // reason, added after the table shipped). Pre-existing databases
        // need the ALTER; fresh databases already have the columns via
        // CREATE_TABLE_USAGE above. Old rows read the columns back as NULL
        // (the read path treats them as `Option`). A failed ALTER does NOT
        // block the connection: the feature degrades — `append_usage`
        // fails loudly on the missing column, transcript operations and
        // the aggregate usage read are unaffected (same philosophy as the
        // sessions migration above).
        {
            let usage_columns: Vec<String> = client
                .query(
                    "SELECT column_name FROM information_schema.columns \
                     WHERE table_name = 'usage_entries'",
                    &[],
                )
                .await
                .context("cannot inspect usage_entries table schema")?
                .iter()
                .map(|row| row.get("column_name"))
                .collect();
            for (column, feature) in [
                ("cache_hit_tokens", "usage cache-hit tokens"),
                ("cache_miss_tokens", "usage cache-miss tokens"),
                ("reasoning_tokens", "usage reasoning tokens"),
            ] {
                if usage_columns.iter().any(|c| c == column) {
                    continue;
                }
                if let Err(error) = client
                    .execute(
                        &format!("ALTER TABLE usage_entries ADD COLUMN {column} BIGINT NULL"),
                        &[],
                    )
                    .await
                {
                    eprintln!(
                        "e-agent: cannot add usage_entries.{column} column ({feature} unavailable): \
                         {error:#}"
                    );
                }
            }
            if !usage_columns.iter().any(|c| c == "finish_reason") {
                match client
                    .execute(
                        "ALTER TABLE usage_entries ADD COLUMN finish_reason STRING NULL",
                        &[],
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(error) => {
                        eprintln!(
                            "e-agent: cannot add usage_entries.finish_reason column \
                             (usage finish reason unavailable): {error:#}"
                        );
                    }
                }
            }
        }

        // Cache the session's latest metadata snapshot (if any) so later
        // touches rewrite complete rows without re-reading immutable
        // columns. The table is append-only; the newest row per session
        // wins (ORDER BY last_active_at DESC LIMIT 1).
        let cached_meta =
            if title_available && pinned_available && archived_available && writer_available {
                let meta_row = client
                    .query_opt(
                        "SELECT created_at, last_active_at, model, \"role\", entry_count, \
                            parent_session_id, parent_task_id, title, pinned, archived, writer \
                     FROM sessions \
                     WHERE workspace_id = $1 AND session_id = $2 \
                     ORDER BY last_active_at DESC LIMIT 1",
                        &[&workspace_id, &session_id],
                    )
                    .await
                    .context("cannot query session metadata")?;
                meta_row.map(|row| SessionMeta {
                    session_id: session_id.to_string(),
                    created_at: row.get("created_at"),
                    last_active_at: row.get("last_active_at"),
                    model: row.get("model"),
                    role: row.get("role"),
                    entry_count: row.get("entry_count"),
                    parent_session_id: row.get("parent_session_id"),
                    parent_task_id: row.get("parent_task_id"),
                    title: row.get("title"),
                    pinned: row.get("pinned"),
                    archived: row.get("archived"),
                    writer: row.get("writer"),
                    label: None, // label lives in running_tasks, resolved at list time
                })
            } else {
                None
            };

        let backend_fp = crate::session_store::backend_instance_fingerprint("greptime", conn);
        let session = Self {
            client,
            next_seq: AtomicI64::new(0),
            workspace_id: workspace_id.to_string(),
            session_id: session_id.to_string(),
            backend_fp,
            cached_meta: std::sync::Mutex::new(cached_meta),
        };

        // Seed next_seq from the DB's current max seq (see db_max_seq).
        let max_seq = session.db_max_seq().await?;
        session.advance_next_seq(max_seq.checked_add(1).context(format!(
            "max_seq overflow in connect for session '{session_id}'"
        ))?);
        Ok(session)
    }

    /// Query the current DB max seq for this session
    /// (`COALESCE(MAX(seq), -1)` → `-1` for an empty partition). Scans the
    /// full session partition, acceptable because sessions are append-only
    /// and bounded by typical turn counts. Used to seed `next_seq` at
    /// connect and to detect concurrent writers before every append.
    async fn db_max_seq(&self) -> Result<i64> {
        let row = self
            .client
            .query_one(
                "SELECT COALESCE(MAX(seq), -1)::BIGINT AS max_seq \
                 FROM session_entries \
                 WHERE workspace_id = $1 AND session_id = $2",
                &[&self.workspace_id, &self.session_id],
            )
            .await
            .context("cannot query max seq")?;
        Ok(row.get("max_seq"))
    }

    /// Advance next_seq from a snapshot length (the number of contiguous
    /// seq 0..N entries that were just loaded).
    ///
    /// - Rejects `len < current next_seq` (rewind / monotonicity violation).
    /// - Rejects lengths that overflow `i64` (impossible for a real session).
    /// - The caller must have verified that the loaded snapshot is a
    ///   complete 0..N range so that `len` is the correct next seq
    ///   (importer TOCTOU re-read pattern).
    pub fn advance_next_seq_from_snapshot_len(&mut self, len: usize) -> Result<()> {
        let next: i64 = i64::try_from(len)
            .context("snapshot length overflowed i64 (impossible for a real session)")?;
        if next < self.next_seq.load(Ordering::Acquire) {
            anyhow::bail!(
                "next_seq rewind: current={}, requested={}; \
                 monotonic advance only (use snapshot length, not DB row count)",
                self.next_seq.load(Ordering::Acquire),
                next,
            );
        }
        self.advance_next_seq(next);
        Ok(())
    }

    /// Advance the append cursor without allowing a concurrent append to
    /// publish a newer value and then be regressed by this operation.
    #[inline]
    fn advance_next_seq(&self, next: i64) {
        advance_cursor(&self.next_seq, next);
    }

    /// The session's live entry count (`next_seq` = `MAX(seq)+1`,
    /// maintained in memory on every append). Cheap metadata for the
    /// sessions list — no DB query, unlike the sessions-table
    /// `entry_count` snapshot which only refreshes on touch.
    pub fn entry_count(&self) -> i64 {
        self.next_seq.load(Ordering::Acquire)
    }

    /// Load all entries for this session, deduplicated by seq (latest
    /// event_time wins) and sorted by winning event_time ASC, then seq ASC.
    /// Delegates to `load_with_seq`.
    pub async fn load(&self) -> Result<Vec<SessionEntry>> {
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
    ///    compared.  If all are identical (idempotent retry at the same
    ///    microsecond), they are folded into one logical entry.  If **any**
    ///    differ, this returns an error with the seq, event_time, session
    ///    id, and manual-inspection guidance.
    ///
    /// 3. Output is ordered by the winning event_time ASC, then seq ASC.
    pub async fn load_with_seq(&self) -> Result<Vec<(i64, SessionEntry)>> {
        let rows = self
            .client
            .query(
                "SELECT seq, event_time, payload FROM session_entries \
                 WHERE workspace_id = $1 AND session_id = $2 \
                 ORDER BY event_time ASC, seq ASC",
                &[&self.workspace_id, &self.session_id],
            )
            .await
            .context("cannot load session entries")?;

        let raw: Vec<(i64, chrono::NaiveDateTime, String)> = rows
            .iter()
            .map(|r| {
                let seq: i64 = r.get("seq");
                let et: chrono::NaiveDateTime = r.get("event_time");
                let p: String = r.get("payload");
                (seq, et, p)
            })
            .collect();

        dedup_raw_entries(&raw, &self.session_id, &self.workspace_id, "event_time")
            .map_err(anyhow::Error::msg)
    }

    /// Load every entry paired with its EXACT physical located key
    /// (`seq` + winning `event_time` + payload hash), deduplicated and
    /// ordered exactly like [`Self::load_with_seq`]. The located key pins
    /// the winning physical row: GreptimeDB's append mode keeps older
    /// same-seq rows, and a receipt issued against `(seq, event_time)`
    /// reads exactly that physical version — a same-seq later write never
    /// retargets it (`read_field` re-checks the payload hash too).
    pub async fn load_located(&self) -> Result<Vec<(EntryLocation, SessionEntry)>> {
        let rows = self
            .client
            .query(
                "SELECT seq, event_time, payload FROM session_entries \
                 WHERE workspace_id = $1 AND session_id = $2 \
                 ORDER BY event_time ASC, seq ASC",
                &[&self.workspace_id, &self.session_id],
            )
            .await
            .context("cannot load session entries")?;

        let raw: Vec<(i64, chrono::NaiveDateTime, String)> = rows
            .iter()
            .map(|r| {
                let seq: i64 = r.get("seq");
                let et: chrono::NaiveDateTime = r.get("event_time");
                let p: String = r.get("payload");
                (seq, et, p)
            })
            .collect();

        let fingerprint = workspace_id_fingerprint(&self.workspace_id);
        let located = dedup_raw_located(&raw, &self.session_id, &self.workspace_id, "event_time")
            .map_err(anyhow::Error::msg)?
            .into_iter()
            .map(|(seq, event_time, entry, raw_payload)| {
                let location = EntryLocation {
                    backend: "greptime",
                    fingerprint: fingerprint.clone(),
                    backend_fp: self.backend_fp.clone(),
                    session: self.session_id.clone(),
                    key: LocatedKey::Greptime {
                        seq,
                        event_time_us: datetime_to_us(event_time),
                    },
                    // Hash the EXACT winning raw payload bytes (the
                    // persisted `payload` string) — the same bytes
                    // `read_field` re-hashes, so a whitespace/key-order
                    // variant can never desync a receipt.
                    entry_hash: entry_payload_hash(&raw_payload),
                };
                Ok((location, entry))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(located)
    }

    /// Direct current-session read by logical seq, selecting the newest row.
    pub async fn read_field_direct(
        &self,
        seq: i64,
        field: crate::output_receipt::FieldId,
    ) -> Result<Vec<u8>> {
        let rows = self.client.query(
            "SELECT payload FROM session_entries WHERE workspace_id = $1 AND session_id = $2 AND seq = $3 ORDER BY event_time DESC LIMIT 1",
            &[&self.workspace_id, &self.session_id, &seq],
        ).await.context("cannot read entry")?;
        let payload: String = rows
            .first()
            .ok_or_else(|| anyhow::anyhow!("entry not found"))?
            .get("payload");
        let entry: SessionEntry = serde_json::from_str(&payload).context("cannot decode entry")?;
        field_bytes(&entry, field).ok_or_else(|| anyhow::anyhow!("entry does not carry that field"))
    }

    /// Exact-version field read: verify the receipt binding against this
    /// store (backend kind, workspace fingerprint, backend-instance
    /// fingerprint, session), SELECT every physical row at the pinned
    /// `seq` + `event_time` (never latest-wins — append mode keeps every
    /// physical row, and identical idempotent-retry duplicates may share
    /// the exact key), select the row whose payload hash matches the
    /// receipt's signed hash (identical duplicates fold), and reject
    /// divergent rows / no match. Re-checks the entry hash, extracts the
    /// field, and rejects total-size drift.
    pub async fn read_field(&self, verified: &VerifiedRef) -> Result<Vec<u8>> {
        let location = &verified.location;
        validate_location_for_store(
            location,
            "greptime",
            &self.workspace_id,
            &self.session_id,
            &self.backend_fp,
        )
        .map_err(anyhow::Error::msg)?;
        let LocatedKey::Greptime { seq, event_time_us } = location.key else {
            return Err(ReceiptError::new(
                ReceiptErrorKind::Integrity,
                "receipt key shape does not match the greptime backend",
            )
            .into());
        };
        let rows = self
            .client
            .query(
                "SELECT payload FROM session_entries \
                 WHERE workspace_id = $1 AND session_id = $2 AND seq = $3 AND event_time = $4",
                &[
                    &self.workspace_id,
                    &self.session_id,
                    &seq,
                    &us_to_datetime(event_time_us),
                ],
            )
            .await
            .context("cannot read pinned entry")?;
        // Fold identical duplicates at the pinned key and select the row
        // matching the receipt's signed entry hash. `query_opt` would pick
        // an ARBITRARY row: with an identical-payload idempotent retry at
        // the same microsecond any row is correct, but a divergent row at
        // the same key must not be able to shadow the signed one (or be
        // silently served).
        let mut signed: Option<String> = None;
        let mut saw_divergent = false;
        for row in &rows {
            let payload: String = row.get("payload");
            if entry_payload_hash(&payload) == location.entry_hash {
                signed = Some(payload);
            } else {
                saw_divergent = true;
            }
        }
        let Some(payload) = signed else {
            return Err(ReceiptError::new(
                ReceiptErrorKind::Integrity,
                format!(
                    "entry not found at pinned location (seq {seq}, event_time_us {event_time_us}) \
                     or its payload changed since the receipt was issued"
                ),
            )
            .into());
        };
        if saw_divergent {
            // The signed row exists, but so does a DIVERGENT row at the
            // exact same key — a foreign writer collided on the same
            // (seq, event_time). Serving the signed row would hide the
            // ambiguity; fail closed instead.
            return Err(ReceiptError::new(
                ReceiptErrorKind::Integrity,
                format!(
                    "pinned location (seq {seq}, event_time_us {event_time_us}) has divergent \
                     physical duplicates; refusing to read an ambiguous entry"
                ),
            )
            .into());
        }
        let entry: SessionEntry = serde_json::from_str(&payload).map_err(|error| {
            ReceiptError::new(
                ReceiptErrorKind::Integrity,
                format!("cannot decode pinned entry: {error}"),
            )
        })?;
        let bytes = field_bytes(&entry, verified.field).ok_or_else(|| {
            ReceiptError::new(
                ReceiptErrorKind::Integrity,
                "pinned entry no longer carries the receipt's field",
            )
        })?;
        if bytes.len() != verified.total {
            return Err(ReceiptError::new(
                ReceiptErrorKind::Integrity,
                format!(
                    "field size changed since the receipt was issued ({} != {})",
                    bytes.len(),
                    verified.total
                ),
            )
            .into());
        }
        Ok(bytes)
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
    pub async fn load_head(&self) -> Result<Vec<SessionEntry>> {
        let row = self
            .client
            .query_opt(
                "SELECT MAX(seq) AS max_seq FROM session_entries \
                 WHERE workspace_id = $1 AND session_id = $2 AND entry_kind = 'compaction'",
                &[&self.workspace_id, &self.session_id],
            )
            .await
            .context("cannot query last compaction seq")?;
        let last_comp: Option<i64> = row.and_then(|r| r.get("max_seq"));
        let Some(last_comp) = last_comp else {
            return self.load().await;
        };

        let rows = self
            .client
            .query(
                "SELECT seq, event_time, payload FROM session_entries \
                 WHERE workspace_id = $1 AND session_id = $2 AND seq >= $3 \
                 ORDER BY event_time ASC, seq ASC",
                &[&self.workspace_id, &self.session_id, &last_comp],
            )
            .await
            .context("cannot load head segment")?;
        let raw: Vec<(i64, chrono::NaiveDateTime, String)> = rows
            .iter()
            .map(|r| {
                let seq: i64 = r.get("seq");
                let et: chrono::NaiveDateTime = r.get("event_time");
                let p: String = r.get("payload");
                (seq, et, p)
            })
            .collect();

        dedup_raw_entries(&raw, &self.session_id, &self.workspace_id, "event_time")
            .map_err(anyhow::Error::msg)
            .map(|v| v.into_iter().map(|(_, e)| e).collect())
    }

    /// The backend seq of the newest compaction entry — the first seq of
    /// the head segment returned by [`Self::load_head`]. `None` when the
    /// session has no compaction at all, i.e. the whole session is one
    /// head segment and there is nothing older to load. The TUI uses this
    /// to seed the first [`Self::load_older`] call:
    /// `load_older(head_seq)` fetches the segment immediately before the
    /// head.
    pub async fn head_seq(&self) -> Result<Option<i64>> {
        let row = self
            .client
            .query_opt(
                "SELECT MAX(seq) AS max_seq FROM session_entries \
                 WHERE workspace_id = $1 AND session_id = $2 AND entry_kind = 'compaction'",
                &[&self.workspace_id, &self.session_id],
            )
            .await
            .context("cannot query last compaction seq")?;
        Ok(row.and_then(|r| r.get("max_seq")))
    }

    /// Load the compaction segment immediately older than `before_seq`:
    /// seq ∈ `[prev_comp, before)` where `prev_comp` is the newest
    /// compaction with `seq < before_seq`. The compaction row that opens
    /// the segment rides with it.
    ///
    /// `limit` enables fixed-size intra-segment paging: when `Some(n)`, the
    /// `n` physical rows closest to `before_seq` (the segment's tail, i.e.
    /// the newest of the segment) are returned instead of the whole segment,
    /// and the cursor is the seq of the oldest row in the page — feed it
    /// back as the next `before_seq` to fetch the next older page *within
    /// the same segment*. Rust best-effort deduplication may leave fewer
    /// logical entries than `n`. This keeps the wire size bounded (~`limit` rows)
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
    /// `limit` counts physical rows, not distinct seqs. Paging uses
    /// `seq >= start AND seq < before_seq ORDER BY seq DESC, event_time DESC
    /// LIMIT n` (the tail of the segment) and reverses the page to ascending
    /// order. Rust then performs best-effort deduplication among the rows
    /// present in that page. Same-seq retries can therefore reduce the
    /// displayed logical entries below `n`; if a page boundary cuts a retry
    /// group, later rows for that seq may be skipped by the seq cursor. This
    /// is acceptable because they represent the same logical entry. The
    /// paged path makes no cross-page exhaustive divergent-tie guarantee.
    pub async fn load_older(
        &self,
        before_seq: i64,
        limit: Option<usize>,
    ) -> Result<(Vec<SessionEntry>, Option<i64>)> {
        let row = self
            .client
            .query_opt(
                "SELECT MAX(seq) AS max_seq FROM session_entries \
                 WHERE workspace_id = $1 AND session_id = $2 \
                 AND entry_kind = 'compaction' AND seq < $3",
                &[&self.workspace_id, &self.session_id, &before_seq],
            )
            .await
            .context("cannot query previous compaction seq")?;
        let prev_comp: Option<i64> = row.and_then(|r| r.get("max_seq"));
        let page = limit.filter(|&n| n > 0);

        let mut raw: Vec<(i64, chrono::NaiveDateTime, String)> = match prev_comp {
            // Middle segment: [prev_comp, before).
            Some(prev) => {
                let rows = if let Some(n) = page {
                    self.client
                        .query(
                            "SELECT seq, event_time, payload \
                             FROM session_entries \
                             WHERE workspace_id = $1 AND session_id = $2 \
                             AND seq >= $3 AND seq < $4 \
                             ORDER BY seq DESC, event_time DESC LIMIT $5",
                            &[
                                &self.workspace_id,
                                &self.session_id,
                                &prev,
                                &before_seq,
                                &(n as i64),
                            ],
                        )
                        .await
                        .context("cannot load middle segment page")?
                } else {
                    self.client
                        .query(
                            "SELECT seq, event_time, payload FROM session_entries \
                             WHERE workspace_id = $1 AND session_id = $2 \
                             AND seq >= $3 AND seq < $4 \
                             ORDER BY event_time ASC, seq ASC",
                            &[&self.workspace_id, &self.session_id, &prev, &before_seq],
                        )
                        .await
                        .context("cannot load middle segment")?
                };
                rows.iter()
                    .map(|r| {
                        let seq: i64 = r.get("seq");
                        let et: chrono::NaiveDateTime = r.get("event_time");
                        let p: String = r.get("payload");
                        (seq, et, p)
                    })
                    .collect()
            }
            // Oldest segment: [0, before). Nothing older follows.
            None => {
                let rows = if let Some(n) = page {
                    self.client
                        .query(
                            "SELECT seq, event_time, payload \
                             FROM session_entries \
                             WHERE workspace_id = $1 AND session_id = $2 AND seq < $3 \
                             ORDER BY seq DESC, event_time DESC LIMIT $4",
                            &[
                                &self.workspace_id,
                                &self.session_id,
                                &before_seq,
                                &(n as i64),
                            ],
                        )
                        .await
                        .context("cannot load oldest segment page")?
                } else {
                    self.client
                        .query(
                            "SELECT seq, event_time, payload FROM session_entries \
                             WHERE workspace_id = $1 AND session_id = $2 AND seq < $3 \
                             ORDER BY event_time ASC, seq ASC",
                            &[&self.workspace_id, &self.session_id, &before_seq],
                        )
                        .await
                        .context("cannot load oldest segment")?
                };
                rows.iter()
                    .map(|r| {
                        let seq: i64 = r.get("seq");
                        let et: chrono::NaiveDateTime = r.get("event_time");
                        let p: String = r.get("payload");
                        (seq, et, p)
                    })
                    .collect()
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

        let entries = dedup_raw_entries(&raw, &self.session_id, &self.workspace_id, "event_time")
            .map_err(anyhow::Error::msg)?
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
    pub async fn load_oldest(&self) -> Result<(Vec<SessionEntry>, Option<i64>)> {
        let row = self
            .client
            .query_opt(
                "SELECT MIN(seq) AS min_seq FROM session_entries \
                 WHERE workspace_id = $1 AND session_id = $2 AND entry_kind = 'compaction'",
                &[&self.workspace_id, &self.session_id],
            )
            .await
            .context("cannot query first compaction seq")?;
        let first_comp: Option<i64> = row.and_then(|r| r.get("min_seq"));
        let Some(first_comp) = first_comp else {
            // No compaction at all: the head segment covers the whole
            // session, so there is nothing older to load.
            return Ok((Vec::new(), None));
        };

        let rows = self
            .client
            .query(
                "SELECT seq, event_time, payload FROM session_entries \
                 WHERE workspace_id = $1 AND session_id = $2 AND seq < $3 \
                 ORDER BY event_time ASC, seq ASC",
                &[&self.workspace_id, &self.session_id, &first_comp],
            )
            .await
            .context("cannot load oldest segment")?;
        let raw: Vec<(i64, chrono::NaiveDateTime, String)> = rows
            .iter()
            .map(|r| {
                let seq: i64 = r.get("seq");
                let et: chrono::NaiveDateTime = r.get("event_time");
                let p: String = r.get("payload");
                (seq, et, p)
            })
            .collect();

        let entries = dedup_raw_entries(&raw, &self.session_id, &self.workspace_id, "event_time")
            .map_err(anyhow::Error::msg)?
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
    pub async fn load_newer(&self, after_seq: i64) -> Result<(Vec<SessionEntry>, Option<i64>)> {
        let row = self
            .client
            .query_opt(
                "SELECT MIN(seq) AS min_seq FROM session_entries \
                 WHERE workspace_id = $1 AND session_id = $2 \
                 AND entry_kind = 'compaction' AND seq > $3",
                &[&self.workspace_id, &self.session_id, &after_seq],
            )
            .await
            .context("cannot query next compaction seq")?;
        let next_comp: Option<i64> = row.and_then(|r| r.get("min_seq"));

        let raw: Vec<(i64, chrono::NaiveDateTime, String)> = match next_comp {
            // Middle segment: [after, next_comp).
            Some(next) => {
                let rows = self
                    .client
                    .query(
                        "SELECT seq, event_time, payload FROM session_entries \
                         WHERE workspace_id = $1 AND session_id = $2 \
                         AND seq >= $3 AND seq < $4 \
                         ORDER BY event_time ASC, seq ASC",
                        &[&self.workspace_id, &self.session_id, &after_seq, &next],
                    )
                    .await
                    .context("cannot load middle segment")?;
                rows.iter()
                    .map(|r| {
                        let seq: i64 = r.get("seq");
                        let et: chrono::NaiveDateTime = r.get("event_time");
                        let p: String = r.get("payload");
                        (seq, et, p)
                    })
                    .collect()
            }
            // Nothing newer: the caller already holds the head segment.
            None => Vec::new(),
        };

        let entries = dedup_raw_entries(&raw, &self.session_id, &self.workspace_id, "event_time")
            .map_err(anyhow::Error::msg)?
            .into_iter()
            .map(|(_, e)| e)
            .collect();
        Ok((entries, next_comp))
    }

    /// Append new entries, returning the exact physical located key of
    /// every appended entry (`seq` + `event_time`, pinned at INSERT time;
    /// GreptimeDB's append mode keeps every physical row, so a receipt
    /// against the returned key always reads exactly this version).
    ///
    /// GreptimeDB's pg-wire does not support transactions, so atomicity
    /// is per statement: a single multi-row INSERT either commits all N rows
    /// or commits zero. Serialization happens before any DB write.
    ///
    /// Chunking: tokio-postgres / pg-wire allows at most 65535 bound
    /// parameters. With 7 params per row, max row count is 65535/7 = 9362.
    /// We use 9000 to leave headroom. >9000-entry appends can partially
    /// commit across chunks; acceptable, turns are far smaller.
    ///
    /// `next_seq` is computed once before any chunk loop. On failure
    /// `next_seq` is unchanged, so retries of the same slice reuse the same
    /// seq range. Identical-payload duplicates from a fully-committed-then-retried
    /// batch are folded by the read path, and the returned located keys stay
    /// exact on that idempotent-retry path (they are re-read from the
    /// committed rows, never freshly generated).
    ///
    /// **Concurrent-write detection (best-effort, fail-closed)**: before
    /// any DB write the current DB max seq is re-read (see
    /// [`Self::db_max_seq`]) and compared against our cursor. GreptimeDB's
    /// pg-wire has NO transactions and NO conditional-write/lease
    /// mechanism, so the pre-check cannot be atomic with the INSERT — a
    /// foreign writer can still race the window. This backend therefore
    /// VERIFIES AFTER committing: every committed row in the written range
    /// is re-read and must still be the physical winner for its seq
    /// (latest `event_time`, identical duplicates folded). If a foreign
    /// writer superseded or diverged any row, the append FAILS CLOSED with
    /// a `concurrent write conflict` error instead of silently
    /// latest-winning or advertising a location that the logical read path
    /// would not pick. This is the strongest guarantee this backend can
    /// provide: the ambiguity is detected and reported, never hidden.
    pub async fn append_located(&self, entries: &[SessionEntry]) -> Result<Vec<EntryLocation>> {
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        let n = entries.len();

        // Validate the complete write range before any DB write.
        // All seqs from base_seq..base_seq+n must be representable as i64
        // and the final cursor must also be in range.
        let base_seq = self.next_seq.load(Ordering::Acquire);
        let n_i64 = i64::try_from(n).context("append count overflowed i64")?;
        let final_cursor = base_seq.checked_add(n_i64).ok_or_else(|| {
            anyhow::anyhow!(
                "seq range overflow: base_seq={base_seq} count={n};                  max seq would exceed i64::MAX"
            )
        })?;

        // Serialize all entries upfront so serialization errors happen
        // before any database write. The event_time pins are generated
        // here too; they become the actual physical values of the INSERTED
        // rows (locations are only built AFTER the commit, from the real
        // physical rows, so a full/partial overlap can never advertise a
        // freshly generated key that was never committed).
        let fingerprint = workspace_id_fingerprint(&self.workspace_id);
        let mut prepped: Vec<(i64, chrono::NaiveDateTime, String, String, bool)> =
            Vec::with_capacity(n);
        for (i, entry) in entries.iter().enumerate() {
            let seq = base_seq + i as i64;
            let payload = serde_json::to_string(entry)
                .with_context(|| format!("cannot serialize entry seq {seq}"))?;
            let kind = entry_kind(entry).to_string();
            let err = is_error(entry);
            let ts = us_to_datetime(next_event_time_us());
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
                // touch within the same nanosecond, and snapshots never
                // overwrite (append-only). In practice, though, the latest
                // writer is almost always the adversary: the losing writer
                // B's touch runs after its own (conflicting) append, so B
                // re-stamps the row with B's identity after A's last
                // snapshot. A failed or empty lookup falls back to the
                // plain message (the `concurrent write conflict` substring
                // is preserved — `friendly_failure` depends on it).
                let writer_hint = self
                    .client
                    .query_opt(
                        "SELECT writer FROM sessions                          WHERE workspace_id = $1 AND session_id = $2                          ORDER BY last_active_at DESC LIMIT 1",
                        &[&self.workspace_id, &self.session_id],
                    )
                    .await
                    .ok()
                    .flatten()
                    .and_then(|row| row.get::<_, Option<String>>("writer"));
                anyhow::bail!(
                    "{}",
                    format_conflict_error(
                        &self.session_id,
                        db_max,
                        base_seq,
                        seq,
                        writer_hint.as_deref(),
                    )
                );
            }
            // The whole overlap matches this batch: it is our own earlier
            // commit (committed-then-errored append whose next_seq never
            // advanced). Treat it as already written — the returned
            // locations are the ACTUAL committed physical rows, re-read
            // from the database (never freshly generated).
            if overlap_len == n {
                let committed = self.read_winning_rows(base_seq, overlap_hi).await?;
                // The DB may have advanced beyond our batch (foreign rows);
                // resume past the true max so we never reuse foreign seqs.
                self.advance_next_seq(db_max.checked_add(1).context(format!(
                    "max_seq overflow advancing next_seq after idempotent retry in session '{}'",
                    self.session_id
                ))?);
                return Ok(committed);
            }
            // Partial overlap: the matching prefix stays committed; insert
            // only the remainder. prepped seqs are base_seq+i, so
            // prepped[overlap_len..] continues contiguously from db_max+1
            // and needs no re-serialization.
            self.insert_prepped(&prepped[overlap_len..]).await?;
            // FAIL-CLOSED post-insert verification: re-read the whole
            // written range and confirm our rows are still the physical
            // winners. A foreign writer that raced the pre-check and
            // committed at our seqs would otherwise leave our advertised
            // locations pointing at rows the logical read path never picks.
            if let Some(seq) = self
                .overlap_conflict_seq(&prepped, base_seq, final_cursor - 1)
                .await?
            {
                anyhow::bail!(
                    "{}",
                    format_conflict_error(&self.session_id, db_max, base_seq, seq, None,)
                );
            }
            // Locations: the accepted prefix comes from the ACTUAL
            // committed rows; the inserted suffix from the exact prepped
            // values (which are the committed physical values).
            let mut committed = self.read_winning_rows(base_seq, overlap_hi).await?;
            committed.extend(prepped_locations(
                &prepped[overlap_len..],
                &fingerprint,
                &self.backend_fp,
                &self.session_id,
            ));
            self.advance_next_seq(final_cursor);
            return Ok(committed);
        }

        self.insert_prepped(&prepped).await?;

        // FAIL-CLOSED post-insert verification (see the partial-overlap
        // path above): our rows must be the physical winners of the whole
        // written range, or the append fails closed.
        if let Some(seq) = self
            .overlap_conflict_seq(&prepped, base_seq, final_cursor - 1)
            .await?
        {
            anyhow::bail!(
                "{}",
                format_conflict_error(&self.session_id, db_max, base_seq, seq, None)
            );
        }

        // Advance next_seq only after all chunks succeed, so a partial
        // failure does not shift sequence numbers on retry.
        self.advance_next_seq(final_cursor);

        // The prepped rows ARE the committed physical rows (no overlap).
        Ok(prepped_locations(
            &prepped,
            &fingerprint,
            &self.backend_fp,
            &self.session_id,
        ))
    }

    /// Append new entries, discarding the located keys (plain-append call
    /// sites that do not issue receipts). See [`Self::append_located`].
    pub async fn append(&self, entries: &[SessionEntry]) -> Result<()> {
        self.append_located(entries).await.map(|_| ())
    }

    /// Insert prepped rows in chunked multi-row INSERTs (see [`Self::append`]
    /// for the chunk-size rationale). Atomicity is per statement: a chunk
    /// either commits all its rows or none.
    async fn insert_prepped(
        &self,
        prepped: &[(i64, chrono::NaiveDateTime, String, String, bool)],
    ) -> Result<()> {
        const CHUNK_SIZE: usize = 9000;
        let wid = &self.workspace_id;
        let sid = &self.session_id;
        for chunk in prepped.chunks(CHUNK_SIZE) {
            let sql = build_multi_row_insert(chunk.len());

            // Flatten params: workspace_id, session_id, seq, ts, kind, payload, is_error per row.
            let mut params: Vec<Box<dyn tokio_postgres::types::ToSql + Sync + Send>> =
                Vec::with_capacity(chunk.len() * 7);
            for (seq, ts, kind, payload, err) in chunk {
                params.push(Box::new(wid.as_str()));
                params.push(Box::new(sid.as_str()));
                params.push(Box::new(*seq));
                params.push(Box::new(*ts));
                params.push(Box::new(kind.as_str()));
                params.push(Box::new(payload.as_str()));
                params.push(Box::new(*err));
            }

            let param_refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = params
                .iter()
                .map(|p| p.as_ref() as &(dyn tokio_postgres::types::ToSql + Sync))
                .collect();
            self.client
                .execute(&sql, &param_refs)
                .await
                .context("cannot append chunk to session_entries")?;
        }
        Ok(())
    }

    /// Insert one token-usage row into the `usage_entries` table. `kind`
    /// is one of "regular" | "compact" | "summarizer".
    ///
    /// TRUE relationship to `session_entries`: `seq` is the `session_entries.seq`
    /// of the just-committed assistant / compaction entry this usage belongs
    /// to (the runner threads it from the commit result), so
    /// `usage_entries.seq == session_entries.seq` is an exact equality join
    /// for "regular" and "compact" rows — NOT a best-effort heuristic. The
    /// row's `event_time` stays the strictly monotonic per-process
    /// microsecond clock ([`next_event_time_us`]) for event-time ordering.
    ///
    /// When `seq` is `None` (the "summarizer" kind has no committed
    /// session entry, and callers may defensively pass `None` on backends
    /// without a usable seq), the event-time clock is used as the seq
    /// instead: the row is event-time-ordered but is NOT seq-joinable to
    /// `session_entries`. The primary key `(workspace_id, session_id, seq)`
    /// stays collision-free within one process (see the `CREATE_TABLE_USAGE`
    /// comment).
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
        seq: Option<i64>,
        usage: &crate::agent::Usage,
    ) -> Result<()> {
        let seq = seq.unwrap_or_else(next_event_time_us);
        let ts = us_to_datetime(next_event_time_us());
        self.client
            .execute(
                "INSERT INTO usage_entries \
                 (workspace_id, session_id, seq, event_time, model, kind, input_tokens, \
                  output_tokens, cache_hit_tokens, cache_miss_tokens, reasoning_tokens, \
                  finish_reason) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
                &[
                    &workspace_id,
                    &session_id,
                    &seq,
                    &ts,
                    &model,
                    &kind,
                    &(usage.input_tokens as i64),
                    &(usage.output_tokens as i64),
                    &usage.cache_hit_tokens.map(|v| v as i64),
                    &usage.cache_miss_tokens.map(|v| v as i64),
                    &usage.reasoning_tokens.map(|v| v as i64),
                    &usage.finish_reason.as_deref(),
                ],
            )
            .await
            .context("cannot insert token usage")?;
        Ok(())
    }

    /// List the most recent finished background tasks for the workspace,
    /// newest first, from `session_entries` rows of
    /// `entry_kind = 'background_completion'` (the authoritative record —
    /// no separate finished-task store). `finished_at` is the row's
    /// `event_time`; `limit` caps the physical rows read (e.g. 100). Rust
    /// best-effort deduplication and fail-closed conflict detection happen
    /// only within that physical-row window, so the result may contain fewer
    /// than `limit` rows and ties outside the window are not inspected. The
    /// payload JSON
    /// deserializes through the same serde shape as `load`, so legacy rows
    /// without the trace fields read back with them `None`.
    pub async fn finished_tasks(
        &self,
        workspace_id: &str,
        limit: usize,
    ) -> Result<Vec<crate::session_store::FinishedTask>> {
        use crate::session_store::FinishedTask;
        // LIMIT applies to physical rows before the best-effort logical-row
        // fold. A retry or duplicate may consume a slot, and a superseded
        // row may be present in the window; this bounds the expensive query.
        let limit_i64 =
            i64::try_from(limit).context("finished task limit does not fit in BIGINT")?;
        let rows = self
            .client
            .query(
                "SELECT session_id, seq, event_time, payload FROM session_entries \
                 WHERE workspace_id = $1 AND entry_kind = 'background_completion' \
                 ORDER BY event_time DESC, seq DESC LIMIT $2",
                &[&workspace_id, &limit_i64],
            )
            .await
            .context("cannot query finished background tasks")?;
        let raw: Vec<(String, i64, chrono::NaiveDateTime, String)> = rows
            .iter()
            .map(|row| {
                (
                    row.get("session_id"),
                    row.get("seq"),
                    row.get("event_time"),
                    row.get("payload"),
                )
            })
            .collect();
        let deduped = dedup_finished_rows(workspace_id, &raw).map_err(anyhow::Error::msg)?;
        let mut out = Vec::with_capacity(deduped.len());
        for (session_id, seq, event_time, payload) in deduped {
            let entry: crate::agent::SessionEntry = serde_json::from_str(&payload)
                .context("cannot decode finished background task payload")?;
            let crate::agent::SessionEntry::BackgroundCompletion {
                id,
                output,
                label,
                started_at_ms,
                duration_ms,
                exit_code,
                signal,
                status,
                kind,
                ..
            } = entry
            else {
                continue;
            };
            out.push(FinishedTask {
                session_id,
                seq,
                finished_at_us: datetime_to_us(event_time),
                id,
                output: crate::session_store::task_output_preview(&output),
                label,
                started_at_ms,
                duration_ms,
                exit_code,
                signal,
                status,
                kind,
            });
        }
        Ok(out)
    }

    /// Aggregate token usage per (session_id, model, kind) for this
    /// workspace: totals plus the first/last event timestamps (µs since
    /// epoch) of each group, newest activity first.
    pub async fn usage_summary(&self) -> Result<Vec<UsageRow>> {
        let rows = self
            .client
            .query(
                "SELECT session_id, model, kind, \
                        SUM(input_tokens) AS input_tokens, SUM(output_tokens) AS output_tokens, \
                        MIN(event_time) AS first_ts, MAX(event_time) AS last_ts \
                 FROM usage_entries \
                 WHERE workspace_id = $1 \
                 GROUP BY session_id, model, kind \
                 ORDER BY last_ts DESC",
                &[&self.workspace_id],
            )
            .await
            .context("cannot query token usage")?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(UsageRow {
                session_id: row.get("session_id"),
                model: row.get("model"),
                kind: row.get("kind"),
                input_tokens: u64::try_from(row.get::<_, i64>("input_tokens"))
                    .context("cannot query token usage: input_tokens is negative")?,
                output_tokens: u64::try_from(row.get::<_, i64>("output_tokens"))
                    .context("cannot query token usage: output_tokens is negative")?,
                first_ts: datetime_to_us(row.get::<_, chrono::NaiveDateTime>("first_ts")),
                last_ts: datetime_to_us(row.get::<_, chrono::NaiveDateTime>("last_ts")),
            });
        }
        Ok(out)
    }

    /// Aggregate token usage per (session_id, model, kind) restricted to
    /// the given session ids (the session itself plus its subagent
    /// children, for the web UI's persisted usage line). Same row shape as
    /// [`usage_summary`]; an empty `session_ids` slice short-circuits to
    /// an empty vector (no query).
    pub async fn usage_for_sessions(&self, session_ids: &[String]) -> Result<Vec<UsageRow>> {
        if session_ids.is_empty() {
            return Ok(Vec::new());
        }
        // $1 = workspace_id，$2.. 依次绑定各会话 id（绑定参数，无注入面）；
        // 同会话+子会话只有个位数 id，语句很短。
        let mut placeholders = String::new();
        for i in 0..session_ids.len() {
            if i > 0 {
                placeholders.push_str(", ");
            }
            placeholders.push_str(&format!("${}", i + 2));
        }
        let sql = format!(
            "SELECT session_id, model, kind, \
                    SUM(input_tokens) AS input_tokens, SUM(output_tokens) AS output_tokens, \
                    MIN(event_time) AS first_ts, MAX(event_time) AS last_ts \
             FROM usage_entries \
             WHERE workspace_id = $1 AND session_id IN ({placeholders}) \
             GROUP BY session_id, model, kind \
             ORDER BY last_ts DESC"
        );
        let mut params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
            Vec::with_capacity(1 + session_ids.len());
        params.push(&self.workspace_id);
        for id in session_ids {
            params.push(id);
        }
        let rows = self
            .client
            .query(&sql, &params)
            .await
            .context("cannot query token usage")?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(UsageRow {
                session_id: row.get("session_id"),
                model: row.get("model"),
                kind: row.get("kind"),
                input_tokens: u64::try_from(row.get::<_, i64>("input_tokens"))
                    .context("cannot query token usage: input_tokens is negative")?,
                output_tokens: u64::try_from(row.get::<_, i64>("output_tokens"))
                    .context("cannot query token usage: output_tokens is negative")?,
                first_ts: datetime_to_us(row.get::<_, chrono::NaiveDateTime>("first_ts")),
                last_ts: datetime_to_us(row.get::<_, chrono::NaiveDateTime>("last_ts")),
            });
        }
        Ok(out)
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
        prepped: &[(i64, chrono::NaiveDateTime, String, String, bool)],
        base_seq: i64,
        overlap_hi: i64,
    ) -> Result<Option<i64>> {
        let rows = self
            .client
            .query(
                "SELECT seq, event_time, payload FROM session_entries \
                 WHERE workspace_id = $1 AND session_id = $2 \
                 AND seq >= $3 AND seq <= $4 \
                 ORDER BY seq ASC, event_time ASC",
                &[&self.workspace_id, &self.session_id, &base_seq, &overlap_hi],
            )
            .await
            .context("cannot read back seq range for concurrent-write check")?;

        // Group by seq, keeping only the row(s) with the latest event_time.
        let mut per_seq: std::collections::HashMap<i64, (chrono::NaiveDateTime, Vec<String>)> =
            std::collections::HashMap::with_capacity(rows.len().min(64));
        for row in &rows {
            let seq: i64 = row.get("seq");
            let et: chrono::NaiveDateTime = row.get("event_time");
            let payload: String = row.get("payload");
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

    /// Read the WINNING physical row of every seq in `[base_seq, overlap_hi]`
    /// (latest `event_time` per seq, identical duplicates folded) and
    /// return its exact location (`seq`, winning `event_time`, hash of the
    /// winning raw payload), ordered by seq. Used by the overlap paths of
    /// [`Self::append_located`] so the returned locations are the ACTUAL
    /// committed physical rows — never freshly generated keys.
    async fn read_winning_rows(
        &self,
        base_seq: i64,
        overlap_hi: i64,
    ) -> Result<Vec<EntryLocation>> {
        let fingerprint = workspace_id_fingerprint(&self.workspace_id);
        let rows = self
            .client
            .query(
                "SELECT seq, event_time, payload FROM session_entries \
                 WHERE workspace_id = $1 AND session_id = $2 \
                 AND seq >= $3 AND seq <= $4 \
                 ORDER BY seq ASC, event_time ASC",
                &[&self.workspace_id, &self.session_id, &base_seq, &overlap_hi],
            )
            .await
            .context("cannot read back committed seq range")?;

        // Group by seq, keeping only the row(s) with the latest event_time.
        let mut per_seq: std::collections::HashMap<i64, (chrono::NaiveDateTime, Vec<String>)> =
            std::collections::HashMap::with_capacity(rows.len().min(64));
        for row in &rows {
            let seq: i64 = row.get("seq");
            let et: chrono::NaiveDateTime = row.get("event_time");
            let payload: String = row.get("payload");
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

        let mut located: Vec<EntryLocation> = per_seq
            .into_iter()
            .map(|(seq, (event_time, payloads))| EntryLocation {
                backend: "greptime",
                fingerprint: fingerprint.clone(),
                backend_fp: self.backend_fp.clone(),
                session: self.session_id.clone(),
                key: LocatedKey::Greptime {
                    seq,
                    event_time_us: datetime_to_us(event_time),
                },
                entry_hash: entry_payload_hash(&payloads[0]),
            })
            .collect();
        located.sort_unstable_by_key(|location| match location.key {
            LocatedKey::Greptime { seq, .. } => seq,
            _ => unreachable!("greptime location"),
        });
        Ok(located)
    }

    // ------------------------------------------------------------------
    // sessions — metadata audit table
    // ------------------------------------------------------------------
    //
    // Greptime has no UPDATE: "upsert" is a same-PK INSERT and
    // last-write-wins by the TIME INDEX. The sessions table is an
    // append-only lifecycle audit log: every create/touch appends a
    // COMPLETE snapshot row (created_at/model/role/parent/entry_count/
    // title/pinned/archived all carried on every row), and the list view
    // deduplicates per session taking the latest last_active_at. There is
    // no partial-row rewrite, so a touch can never wipe immutable columns.

    /// Latest metadata snapshot for one session, or `None` when the
    /// session has no row yet (brand-new session, or a subagent whose
    /// parent has not written its row yet).
    async fn load_meta_row(&self, session_id: &str) -> Result<Option<SessionMeta>> {
        let row = self
            .client
            .query_opt(
                "SELECT created_at, last_active_at, model, \"role\", entry_count, \
                        parent_session_id, parent_task_id, title, pinned, archived, writer \
                 FROM sessions \
                 WHERE workspace_id = $1 AND session_id = $2 \
                 ORDER BY last_active_at DESC LIMIT 1",
                &[&self.workspace_id, &session_id],
            )
            .await
            .context("cannot query session metadata")?;
        Ok(row.map(|row| SessionMeta {
            session_id: session_id.to_owned(),
            created_at: row.get("created_at"),
            last_active_at: row.get("last_active_at"),
            model: row.get("model"),
            role: row.get("role"),
            entry_count: row.get("entry_count"),
            parent_session_id: row.get("parent_session_id"),
            parent_task_id: row.get("parent_task_id"),
            title: row.get("title"),
            pinned: row.get("pinned"),
            archived: row.get("archived"),
            writer: row.get("writer"),
            label: None, // label lives in running_tasks, resolved at list time
        }))
    }

    async fn insert_meta(&self, meta: &mut SessionMeta) -> Result<()> {
        // Stamp the writer identity at insert time, not at construction
        // (P3): every snapshot row records the process that actually wrote
        // it, and a cross-process resume never replays a stale identity
        // from a row cached by another process. Callers construct with
        // `writer: None`; the stamped value is what lands in the column.
        meta.writer = Some(process_identity().to_owned());
        self.client
            .execute(
                "INSERT INTO sessions \
                 (workspace_id, session_id, created_at, last_active_at, model, \"role\", \
                  entry_count, parent_session_id, parent_task_id, title, pinned, archived, writer) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
                &[
                    &self.workspace_id,
                    &meta.session_id,
                    &meta.created_at,
                    &meta.last_active_at,
                    &meta.model,
                    &meta.role,
                    &meta.entry_count,
                    &meta.parent_session_id,
                    &meta.parent_task_id,
                    &meta.title,
                    &meta.pinned,
                    &meta.archived,
                    &meta.writer,
                ],
            )
            .await
            .context("cannot insert session metadata")?;
        Ok(())
    }

    /// Read the current full snapshot for the bound session. Mutations read
    /// the database even when this instance has a cache: another
    /// GreptimeSession may have deleted or replaced the row while this
    /// instance was idle. Metadata writes intentionally use the database's
    /// optimistic last-write-wins semantics. Delete-vs-touch races remain a
    /// documented limitation until database-level conditional/versioned
    /// writes are available.
    async fn effective_meta(&self) -> Result<Option<SessionMeta>> {
        match self.load_meta_row(&self.session_id).await? {
            Some(meta) => {
                *self.cached_meta.lock().unwrap() = Some(meta.clone());
                Ok(Some(meta))
            }
            None => {
                *self.cached_meta.lock().unwrap() = None;
                Ok(None)
            }
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
    ) -> Result<()> {
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
            entry_count: self.next_seq.load(Ordering::Acquire),
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
    pub async fn touch_meta(&self) -> Result<()> {
        let Some(meta) = self.effective_meta().await? else {
            return Ok(());
        };
        let mut meta = meta;
        meta.last_active_at = us_to_datetime(next_event_time_us());
        meta.entry_count = self.next_seq.load(Ordering::Acquire);
        self.insert_meta(&mut meta).await?;
        *self.cached_meta.lock().unwrap() = Some(meta);
        Ok(())
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
    pub async fn set_title(&self, session_id: &str, title: Option<&str>) -> Result<()> {
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
    pub async fn set_pinned(&self, session_id: &str, pinned: bool) -> Result<()> {
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
    pub async fn set_archived(&self, session_id: &str, archived: bool) -> Result<()> {
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
    pub async fn list_meta(&self) -> Result<Vec<SessionMeta>> {
        Ok(self.list_meta_diagnostic().await?.0)
    }

    pub async fn list_meta_diagnostic(
        &self,
    ) -> Result<(Vec<SessionMeta>, crate::session_store::ListMetaDiagnostics)> {
        let query_started = std::time::Instant::now();
        let rows = self
            .client
            .query(
                "SELECT s.session_id, s.created_at, s.last_active_at, s.model, s.\"role\", \
                        s.entry_count, s.parent_session_id, s.parent_task_id, s.title, s.pinned, \
                        s.archived, s.writer \
                 FROM sessions s \
                 INNER JOIN ( \
                     SELECT session_id, MAX(last_active_at) AS max_ts \
                     FROM sessions WHERE workspace_id = $1 GROUP BY session_id \
                 ) latest \
                   ON latest.session_id = s.session_id AND latest.max_ts = s.last_active_at \
                 WHERE s.workspace_id = $1 \
                 ORDER BY s.last_active_at DESC",
                &[&self.workspace_id],
            )
            .await
            .context("cannot list session metadata")?;
        let query_ms = query_started.elapsed().as_millis();
        let decode_started = std::time::Instant::now();
        let out = rows
            .iter()
            .map(|row| SessionMeta {
                session_id: row.get("session_id"),
                created_at: row.get("created_at"),
                last_active_at: row.get("last_active_at"),
                model: row.get("model"),
                role: row.get("role"),
                entry_count: row.get("entry_count"),
                parent_session_id: row.get("parent_session_id"),
                parent_task_id: row.get("parent_task_id"),
                title: row.get("title"),
                pinned: row.get("pinned"),
                archived: row.get("archived"),
                writer: row.get("writer"),
                label: None, // label lives in running_tasks, resolved at list time
            })
            .collect::<Vec<_>>();
        let decode_ms = decode_started.elapsed().as_millis();
        Ok((
            out.clone(),
            crate::session_store::ListMetaDiagnostics {
                backend: "greptime",
                query_ms,
                row_decode_ms: decode_ms,
                logical_rows: out.len(),
                ..Default::default()
            },
        ))
    }

    /// The full lifecycle trace of one session: every snapshot row, oldest
    /// activity first. This is the audit view — [`Self::list_meta`] shows
    /// only the latest snapshot per session.
    pub async fn audit_meta(&self, session_id: &str) -> Result<Vec<SessionMeta>> {
        let rows = self
            .client
            .query(
                "SELECT created_at, last_active_at, model, \"role\", entry_count, \
                        parent_session_id, parent_task_id, title, pinned, archived, writer \
                 FROM sessions \
                 WHERE workspace_id = $1 AND session_id = $2 \
                 ORDER BY last_active_at ASC",
                &[&self.workspace_id, &session_id],
            )
            .await
            .context("cannot load session metadata audit trail")?;
        Ok(rows
            .iter()
            .map(|row| SessionMeta {
                session_id: session_id.to_owned(),
                created_at: row.get("created_at"),
                last_active_at: row.get("last_active_at"),
                model: row.get("model"),
                role: row.get("role"),
                entry_count: row.get("entry_count"),
                parent_session_id: row.get("parent_session_id"),
                parent_task_id: row.get("parent_task_id"),
                title: row.get("title"),
                pinned: row.get("pinned"),
                archived: row.get("archived"),
                writer: row.get("writer"),
                label: None, // label lives in running_tasks, resolved at list time
            })
            .collect())
    }

    /// Hide a session from the sessions list: delete ALL of its metadata
    /// rows (the complete audit trail). Resume still works because
    /// `session_entries` is untouched. Known limitation (documented, per
    /// the audit-log design without tombstones): a later
    /// [`Self::backfill_sessions`] bootstrap run re-creates the row from
    /// the transcript, so hiding is scoped to the current server lifetime.
    pub async fn delete_meta(&self, session_id: &str) -> Result<()> {
        self.client
            .execute(
                "DELETE FROM sessions WHERE workspace_id = $1 AND session_id = $2",
                &[&self.workspace_id, &session_id],
            )
            .await
            .context("cannot delete session metadata")?;
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
    pub async fn backfill_sessions(&self) -> Result<()> {
        let existing: Vec<String> = self
            .client
            .query(
                "SELECT DISTINCT session_id FROM sessions WHERE workspace_id = $1",
                &[&self.workspace_id],
            )
            .await
            .context("cannot query existing session metadata")?
            .iter()
            .map(|row| row.get("session_id"))
            .collect();
        let aggregates = self
            .client
            .query(
                "SELECT session_id, MAX(seq) + 1 AS entry_count, \
                        MIN(event_time) AS created_at, MAX(event_time) AS last_active_at \
                 FROM session_entries \
                 WHERE workspace_id = $1 \
                 GROUP BY session_id",
                &[&self.workspace_id],
            )
            .await
            .context("cannot aggregate session_entries for metadata backfill")?;
        for row in aggregates {
            let session_id: String = row.get("session_id");
            if existing.iter().any(|id| id == &session_id) {
                continue;
            }
            self.insert_meta(&mut SessionMeta {
                session_id,
                created_at: row.get("created_at"),
                last_active_at: row.get("last_active_at"),
                model: None,
                role: None,
                entry_count: row.get("entry_count"),
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
    // These are the GreptimeDB counterpart of the JSONL
    // `*.background.jsonl` record files (`session::Session`). Unlike the
    // transcript, they are keyed by the *recording* session (the one whose
    // task list owns the row) and may be written by a store bound to a
    // different transcript session (a delegate row lives under the parent
    // session, with the subagent session id in `subagent_session_id`).
    // That is why every method takes the session id explicitly instead of
    // reusing the bound `self.session_id`.

    /// Record a freshly started background task so a later launch can tell
    /// the user what died with the previous process.
    ///
    /// `full_command` 是完整命令原文（bash 任务传 `Some`，delegate 无命令传
    /// `None`），持久化到 `running_tasks.full_command`，供 `/api/tasks`
    /// 在 live registry 缺失时回退取用。
    pub async fn record_task_start(
        &self,
        session_id: &str,
        task_id: u64,
        label: &str,
        full_command: Option<&str>,
        subagent_session_id: Option<&str>,
    ) -> Result<()> {
        let started_at = us_to_datetime(next_event_time_us());
        let task_id =
            i64::try_from(task_id).context("background task id does not fit in BIGINT")?;
        self.client
            .execute(
                "INSERT INTO running_tasks \
                 (workspace_id, session_id, task_id, label, full_command, subagent_session_id, started_at, owner_identity) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                &[
                    &self.workspace_id,
                    &session_id,
                    &task_id,
                    &label,
                    &full_command,
                    &subagent_session_id,
                    &started_at,
                    &crate::session_store::process_identity(),
                ],
            )
            .await
            .context("cannot record background task start")?;
        Ok(())
    }

    /// Forget one task: its completion arrived while the process was alive.
    pub async fn clear_task(&self, session_id: &str, task_id: u64) -> Result<()> {
        let task_id =
            i64::try_from(task_id).context("background task id does not fit in BIGINT")?;
        self.client
            .execute(
                "DELETE FROM running_tasks \
                 WHERE workspace_id = $1 AND session_id = $2 AND task_id = $3",
                &[&self.workspace_id, &session_id, &task_id],
            )
            .await
            .context("cannot clear background task")?;
        Ok(())
    }

    /// Look up one surviving row's full command by task id (the `/api/tasks`
    /// fallback when the live registry lacks it). `None` when the row is
    /// gone (consumed/completed) or its `full_command` is NULL (delegate
    /// rows, pre-migration rows — 缺失 → None 兼容旧数据).
    pub async fn task_full_command(
        &self,
        session_id: &str,
        task_id: u64,
    ) -> Result<Option<String>> {
        let task_id =
            i64::try_from(task_id).context("background task id does not fit in BIGINT")?;
        let rows = self
            .client
            .query(
                "SELECT full_command FROM running_tasks \
                 WHERE workspace_id = $1 AND session_id = $2 AND task_id = $3",
                &[&self.workspace_id, &session_id, &task_id],
            )
            .await
            .context("cannot load background task full command")?;
        if rows.is_empty() {
            return Ok(None);
        }
        Ok(rows[0].get("full_command"))
    }

    /// Tasks recorded by a previous process that died before their
    /// completion arrived. Consumes (deletes) all rows for the session and
    /// returns the labels so the caller can inject the "killed on exit"
    /// notice. Rows scoped to another session are untouched.
    pub(crate) async fn peek_unfinished_tasks(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::session::UnfinishedTask>> {
        let rows = self.client.query("SELECT task_id, label, subagent_session_id, started_at, owner_identity FROM running_tasks WHERE workspace_id = $1 AND session_id = $2", &[&self.workspace_id, &session_id]).await.context("cannot load unfinished background tasks")?;
        Ok(rows
            .iter()
            .map(|row| crate::session::UnfinishedTask {
                task_id: row.get::<_, i64>("task_id").max(0) as u64,
                label: row.get("label"),
                subagent_session_id: row.get("subagent_session_id"),
                session_id: Some(session_id.to_owned()),
                started_at: Some(crate::session_store::datetime_to_us(row.get("started_at"))),
                raw: None,
                owner_identity: row.get("owner_identity"),
            })
            .collect())
    }

    pub(crate) async fn consume_unfinished_tasks(
        &self,
        session_id: &str,
        tasks: &[crate::session::UnfinishedTask],
    ) -> Result<(), String> {
        for task in tasks {
            let changed = if let Some(subagent) = task.subagent_session_id.as_deref() {
                self.client.execute("DELETE FROM running_tasks WHERE workspace_id = $1 AND session_id = $2 AND task_id = $3 AND label = $4 AND subagent_session_id = $5 AND started_at = $6", &[&self.workspace_id, &session_id, &(task.task_id as i64), &task.label, &subagent, &crate::session_store::us_to_datetime(task.started_at.unwrap_or_default())]).await
            } else {
                self.client.execute("DELETE FROM running_tasks WHERE workspace_id = $1 AND session_id = $2 AND task_id = $3 AND label = $4 AND subagent_session_id IS NULL AND started_at = $5", &[&self.workspace_id, &session_id, &(task.task_id as i64), &task.label, &crate::session_store::us_to_datetime(task.started_at.unwrap_or_default())]).await
            }.map_err(|e| format!("cannot clear unfinished background tasks: {e}"))?;
            if changed == 0 {
                return Err("unfinished background task consume matched no row".into());
            }
        }
        Ok(())
    }

    pub(crate) async fn consume_unfinished_tasks_for_subagent(
        &self,
        id: &str,
        tasks: &[crate::session::UnfinishedTask],
    ) -> Result<(), String> {
        for task in tasks {
            let Some(parent_session_id) = task.session_id.as_deref() else {
                continue;
            };
            let changed = self.client.execute("DELETE FROM running_tasks WHERE workspace_id = $1 AND session_id = $2 AND subagent_session_id = $3 AND task_id = $4 AND label = $5 AND started_at = $6", &[&self.workspace_id, &parent_session_id, &id, &(task.task_id as i64), &task.label, &crate::session_store::us_to_datetime(task.started_at.unwrap_or_default())]).await.map_err(|e| format!("cannot clear unfinished subagent tasks: {e}"))?;
            if changed == 0 {
                return Err("unfinished subagent task consume matched no row".into());
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub async fn take_unfinished_tasks(&self, id: &str) -> Result<Vec<String>> {
        let tasks = self.peek_unfinished_tasks(id).await?;
        let labels = tasks
            .iter()
            .map(|task| {
                crate::session::format_unfinished(
                    task.task_id,
                    &task.label,
                    task.subagent_session_id.as_deref(),
                )
            })
            .collect();
        if !tasks.is_empty() {
            self.consume_unfinished_tasks(id, &tasks)
                .await
                .map_err(anyhow::Error::msg)?;
        }
        Ok(labels)
    }

    #[cfg(test)]
    pub async fn take_unfinished_tasks_for_subagent(&self, id: &str) -> Result<Vec<String>> {
        let tasks = self.peek_unfinished_tasks_for_subagent(id).await?;
        let labels = tasks
            .iter()
            .map(|task| crate::session::format_unfinished(task.task_id, &task.label, None))
            .collect();
        if !tasks.is_empty() {
            self.consume_unfinished_tasks_for_subagent(id, &tasks)
                .await
                .map_err(anyhow::Error::msg)?;
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
    pub async fn unfinished_owner_all_dead(&self, session_id: &str) -> Result<bool> {
        let rows = self
            .client
            .query(
                "SELECT owner_identity FROM running_tasks \
                 WHERE workspace_id = $1 AND session_id = $2",
                &[&self.workspace_id, &session_id],
            )
            .await
            .context("cannot load unfinished background task owners")?;
        for row in rows.iter() {
            let owner: Option<String> = row.get("owner_identity");
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

    pub(crate) async fn unfinished_owner_all_dead_for_subagent(&self, id: &str) -> Result<bool> {
        let rows = self.client.query("SELECT owner_identity FROM running_tasks WHERE workspace_id = $1 AND subagent_session_id = $2", &[&self.workspace_id, &id]).await.context("cannot load unfinished subagent task owners")?;
        for row in rows {
            let owner: Option<String> = row.get("owner_identity");
            match owner {
                Some(owner) if !crate::session_store::owner_alive(&owner) => {}
                _ => return Ok(false),
            }
        }
        Ok(true)
    }

    /// Same as [`Self::take_unfinished_tasks`] but keyed by
    /// `subagent_session_id`: the rows a killed parent left for one of its
    /// background delegate subagents. The table is global (unlike JSONL
    /// per-session files), so a resumed subagent can look up its own
    /// leftovers from any parent session. The subagent session id is
    /// implied by the lookup, so labels carry no `(session: …)` suffix.
    pub(crate) async fn peek_unfinished_tasks_for_subagent(
        &self,
        subagent_session_id: &str,
    ) -> Result<Vec<crate::session::UnfinishedTask>> {
        let rows = self.client.query("SELECT task_id, label, session_id, started_at, owner_identity FROM running_tasks WHERE workspace_id = $1 AND subagent_session_id = $2", &[&self.workspace_id, &subagent_session_id]).await.context("cannot load unfinished subagent tasks")?;
        Ok(rows
            .iter()
            .map(|row| crate::session::UnfinishedTask {
                task_id: row.get::<_, i64>("task_id").max(0) as u64,
                label: row.get("label"),
                subagent_session_id: Some(subagent_session_id.to_owned()),
                session_id: Some(row.get("session_id")),
                started_at: Some(crate::session_store::datetime_to_us(row.get("started_at"))),
                raw: None,
                owner_identity: row.get("owner_identity"),
            })
            .collect())
    }

    /// The task-panel label for a subagent session: the label of the newest
    /// `running_tasks` row whose `subagent_session_id` matches. A subagent
    /// can have several rows (one per delegate task, possibly from
    /// different parents); the most recently started survives. Rows are
    /// consumed when the task completes, so this returns `None` for a
    /// subagent with no live delegate task. Non-destructive (unlike the
    /// `take_unfinished_*` lookups) — called from `/api/sessions` listing.
    pub async fn label_for_subagent(&self, subagent_session_id: &str) -> Result<Option<String>> {
        let row = self
            .client
            .query_opt(
                "SELECT label FROM running_tasks \
                 WHERE workspace_id = $1 AND subagent_session_id = $2 \
                 ORDER BY started_at DESC LIMIT 1",
                &[&self.workspace_id, &subagent_session_id],
            )
            .await
            .context("cannot look up subagent task label")?;
        Ok(row.map(|row| row.get("label")))
    }

    /// The batched form of [`Self::label_for_subagent`], used by the
    /// sessions list to resolve every subagent label in ONE query instead
    /// of a per-subagent N+1. Returns the full
    /// `subagent_session_id → label` map in a single scan. A subagent can
    /// have several rows (one per delegate task, possibly from different
    /// parents); rows are inserted oldest→newest so each later insert
    /// overwrites the previous one and `ORDER BY started_at ASC` makes
    /// the newest label the final (winning) value — the same "newest
    /// wins" rule as the per-session lookup. Non-destructive (unlike the
    /// `take_unfinished_*` lookups) — called from `/api/sessions`
    /// listing. A subagent with no live delegate task is simply absent
    /// from the map (the caller reads it as `None`).
    pub async fn all_subagent_labels(&self) -> Result<HashMap<String, Option<String>>> {
        let rows = self
            .client
            .query(
                "SELECT subagent_session_id, label FROM running_tasks \
                 WHERE workspace_id = $1 AND subagent_session_id IS NOT NULL \
                 ORDER BY started_at ASC",
                &[&self.workspace_id],
            )
            .await
            .context("cannot load all subagent task labels")?;
        let mut labels = HashMap::new();
        for row in rows {
            let Some(subagent_session_id) = row.get::<_, Option<String>>("subagent_session_id")
            else {
                // NULL subagent_session_id is excluded by the WHERE clause;
                // skip defensively instead of failing the whole batch.
                continue;
            };
            let label = row.get::<_, Option<String>>("label");
            labels.insert(subagent_session_id, label);
        }
        Ok(labels)
    }
}

/// Build a multi-row INSERT with 7 bound parameters per row (workspace_id,
/// session_id, seq, event_time, entry_kind, payload, is_error);
/// schema_version is hardcoded as 1.
/// Example (1 row):
/// `INSERT INTO session_entries (workspace_id, session_id, seq, event_time,
///  entry_kind, payload, schema_version, is_error) VALUES ($1,$2,$3,$4,$5,$6,1,$7)`
fn build_multi_row_insert(row_count: usize) -> String {
    let mut sql = String::with_capacity(200 + row_count * 50);
    sql.push_str(
        "INSERT INTO session_entries \
         (workspace_id, session_id, seq, event_time, entry_kind, payload, \
          schema_version, is_error) VALUES ",
    );
    for i in 0..row_count {
        if i > 0 {
            sql.push(',');
        }
        let b = i * 7;
        sql.push_str(&format!(
            "(${b1},${b2},${b3},${b4},${b5},${b6},1,${b7})",
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

/// Build the exact locations of prepped rows that were INSERTED with those
/// exact values (the prepped `seq`/`event_time`/`payload` ARE the committed
/// physical row values). `backend_fp` is the store's instance fingerprint.
fn prepped_locations(
    prepped: &[(i64, chrono::NaiveDateTime, String, String, bool)],
    fingerprint: &str,
    backend_fp: &str,
    session_id: &str,
) -> Vec<EntryLocation> {
    prepped
        .iter()
        .map(|(seq, ts, _, payload, _)| EntryLocation {
            backend: "greptime",
            fingerprint: fingerprint.to_owned(),
            backend_fp: backend_fp.to_owned(),
            session: session_id.to_owned(),
            key: LocatedKey::Greptime {
                seq: *seq,
                event_time_us: datetime_to_us(*ts),
            },
            entry_hash: entry_payload_hash(payload),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AssistantMessage, Message};
    use std::path::Path;

    #[test]
    fn greptime_session_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GreptimeSession>();
    }

    #[test]
    fn cursor_advancement_is_monotonic_under_concurrency() {
        use std::sync::Arc;
        let cursor = Arc::new(AtomicI64::new(0));
        std::thread::scope(|scope| {
            for next in [3, 11, 7, 19, 5, 23, 13, 29] {
                let cursor = Arc::clone(&cursor);
                scope.spawn(move || advance_cursor(&cursor, next));
            }
        });
        assert_eq!(cursor.load(Ordering::Acquire), 29);
    }

    fn conn_str() -> String {
        std::env::var("GREPTIME_PG").unwrap_or_else(|_| {
            // No env var = no live DB; exit(0) means skip for the
            // connection integration tests, but unit tests below still pass.
            "skipped".into()
        })
    }

    fn workspace_id() -> String {
        derive_workspace_id(Path::new("/tmp/e-agent-test"))
    }

    fn finished_workspace_id() -> String {
        derive_workspace_id(Path::new(&format!(
            "/tmp/e-agent-test-finished-{}",
            crate::session::new_id()
        )))
    }

    fn test_entries() -> Vec<SessionEntry> {
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
            Message::Tool {
                call_id: "call_1".into(),
                name: "bash".into(),
                content: "exit code: 0\nstdout:\nok\n".into(),
                is_error: false,
                synthetic: false,
                images: vec![],
            }
            .into(),
            Message::Tool {
                call_id: "call_err".into(),
                name: "bash".into(),
                content: "command not found".into(),
                is_error: true,
                synthetic: false,
                images: vec![],
            }
            .into(),
            SessionEntry::Compaction {
                summary: "## summary text".into(),
                retained: vec![
                    Message::User {
                        content: "retained user".into(),
                        images: vec![],
                    },
                    Message::Assistant(AssistantMessage {
                        content: Some("retained assistant".into()),
                        tool_calls: vec![],
                        reasoning: None,
                    }),
                ],
                current_prompt_at: None,
                no_current_prompt: false,
            },
            SessionEntry::Notice {
                text: "[background task 1 completed]\nexit: 0".into(),
            },
            SessionEntry::BackgroundCompletion {
                id: 2,
                output: "exit code: 0\nstdout:\nbuilt successfully\nstderr:\n".into(),
                label: None,
                started_at_ms: None,
                duration_ms: None,
                exit_code: None,
                signal: None,
                status: None,
                kind: None,
            },
            // Entry with a label to verify serde roundtrip with label.
            SessionEntry::BackgroundCompletion {
                id: 3,
                output: "some output".into(),
                label: Some("build project".into()),
                started_at_ms: None,
                duration_ms: None,
                exit_code: None,
                signal: None,
                status: None,
                kind: None,
            },
            Message::User {
                content: "你好世界👋\n多行".into(),
                images: vec![],
            }
            .into(),
            Message::User {
                content: "'); DROP TABLE session_entries; --".into(),
                images: vec![],
            }
            .into(),
        ]
    }

    #[tokio::test]
    async fn roundtrip() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        let entries = test_entries();
        session.append(&entries).await.unwrap();

        let loaded = session.load().await.unwrap();
        assert_eq!(loaded.len(), entries.len());
        for (got, want) in loaded.iter().zip(entries.iter()) {
            assert_eq!(got, want);
        }
    }

    #[tokio::test]
    async fn finished_tasks_reads_background_completion_entries_newest_first() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = finished_workspace_id();
        let sid = format!("test-gt-finished-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        let entries = vec![
            SessionEntry::BackgroundCompletion {
                id: 1,
                output: "old".into(),
                label: None,
                started_at_ms: None,
                duration_ms: None,
                exit_code: None,
                signal: None,
                status: None,
                kind: None,
            },
            SessionEntry::BackgroundCompletion {
                id: 2,
                output: "exit code: 3\nstdout:\n\nstderr:\n".into(),
                label: Some("build".into()),
                started_at_ms: Some(1_700_000_000_000),
                duration_ms: Some(500),
                exit_code: Some(3),
                signal: None,
                status: Some("failed".into()),
                kind: Some("bash".into()),
            },
        ];
        session.append(&entries).await.unwrap();

        let finished = session.finished_tasks(&wid, 100).await.unwrap();
        assert_eq!(finished.len(), 2);
        assert_eq!(finished[0].id, 2, "newest first");
        assert_eq!(finished[0].exit_code, Some(3));
        assert_eq!(finished[0].status.as_deref(), Some("failed"));
        assert_eq!(finished[0].kind.as_deref(), Some("bash"));
        assert_eq!(finished[0].duration_ms, Some(500));
        assert_eq!(finished[1].exit_code, None, "legacy row reads back NULL");
        assert_eq!(finished[1].status, None);
        let limited = session.finished_tasks(&wid, 1).await.unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].id, 2);
    }

    #[tokio::test]
    async fn finished_tasks_dedups_retried_and_superseded_rows() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = finished_workspace_id();
        let sid = format!("test-gt-finished-dedup-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        // One logical completion (id=1), then a superseded retry of the
        // SAME seq: the retry commit writes a newer physical row for seq 0
        // with a different payload (id=2). A second completion lands at
        // seq 1. Physical rows for seq 0: 2 (one superseded, one winning).
        let first = serde_json::to_string(&SessionEntry::BackgroundCompletion {
            id: 1,
            output: "v1".into(),
            label: None,
            started_at_ms: None,
            duration_ms: None,
            exit_code: None,
            signal: None,
            status: Some("completed".into()),
            kind: Some("bash".into()),
        })
        .unwrap();
        let retried = serde_json::to_string(&SessionEntry::BackgroundCompletion {
            id: 2,
            output: "v2".into(),
            label: None,
            started_at_ms: None,
            duration_ms: None,
            exit_code: Some(3),
            signal: None,
            status: Some("failed".into()),
            kind: Some("bash".into()),
        })
        .unwrap();
        let second = serde_json::to_string(&SessionEntry::BackgroundCompletion {
            id: 3,
            output: "latest".into(),
            label: None,
            started_at_ms: None,
            duration_ms: None,
            exit_code: Some(0),
            signal: None,
            status: Some("completed".into()),
            kind: Some("bash".into()),
        })
        .unwrap();
        let _ = &first;
        // Write rows directly so we control seqs/event_times: an older
        // event_time for seq 0 (superseded), a NEWER event_time for seq 0
        // (the retry that wins), and seq 1 (newest completion).
        let sql = "INSERT INTO session_entries                    (workspace_id, session_id, seq, event_time, entry_kind, payload,                     schema_version, is_error)                    VALUES ($1, $2, $3, $4, $5, $6, 1, false)";
        session
            .client
            .execute(
                sql,
                &[
                    &wid,
                    &sid,
                    &0i64,
                    &crate::session_store::us_to_datetime(1_700_000_000_000_000i64),
                    &"background_completion",
                    &first,
                ],
            )
            .await
            .unwrap();
        session
            .client
            .execute(
                sql,
                &[
                    &wid,
                    &sid,
                    &0i64,
                    &crate::session_store::us_to_datetime(1_700_000_000_001_000i64),
                    &"background_completion",
                    &retried,
                ],
            )
            .await
            .unwrap();
        session
            .client
            .execute(
                sql,
                &[
                    &wid,
                    &sid,
                    &1i64,
                    &crate::session_store::us_to_datetime(1_700_000_000_002_000i64),
                    &"background_completion",
                    &second,
                ],
            )
            .await
            .unwrap();

        // Dedup folds the two seq-0 physical rows into one LOGICAL row;
        // the superseded id=1 row must never appear.
        let finished = session.finished_tasks(&wid, 100).await.unwrap();
        assert_eq!(finished.len(), 2, "3 physical rows -> 2 logical rows");
        assert_eq!(finished[0].id, 3, "newest first");
        assert_eq!(
            finished[1].id, 2,
            "retry winner at seq 0, NOT the superseded id=1"
        );
        assert_eq!(finished[1].status.as_deref(), Some("failed"));
        assert_eq!(finished[1].exit_code, Some(3));

        // The physical window contains the newest row, so limit=1 still
        // returns the newest completion. A superseded row outside that
        // window is not guaranteed to be discovered by this best-effort fold.
        let limited = session.finished_tasks(&wid, 1).await.unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].id, 3);
    }

    #[tokio::test]
    async fn usage_entries_append_and_summarize() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-usage-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        // 空表 → 空汇总。
        assert!(session.usage_summary().await.unwrap().is_empty());

        // 同一 (session, model, kind) 多行聚合；不同 model/kind 维度分行。
        session
            .append_usage(
                &wid,
                &sid,
                "model-a",
                "regular",
                None,
                &crate::agent::Usage {
                    input_tokens: 100,
                    output_tokens: 50,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        session
            .append_usage(
                &wid,
                &sid,
                "model-a",
                "regular",
                None,
                &crate::agent::Usage {
                    input_tokens: 200,
                    output_tokens: 30,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        session
            .append_usage(
                &wid,
                &sid,
                "model-a",
                "compact",
                None,
                &crate::agent::Usage {
                    input_tokens: 1000,
                    output_tokens: 200,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        session
            .append_usage(
                &wid,
                &sid,
                "model-b",
                "regular",
                None,
                &crate::agent::Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let rows = session.usage_summary().await.unwrap();
        assert_eq!(rows.len(), 3);
        let regular_a = rows
            .iter()
            .find(|r| r.model == "model-a" && r.kind == "regular")
            .expect("regular/model-a group");
        assert_eq!(regular_a.session_id, sid);
        assert_eq!(regular_a.input_tokens, 300);
        assert_eq!(regular_a.output_tokens, 80);
        assert!(regular_a.first_ts <= regular_a.last_ts);
        let compact_a = rows
            .iter()
            .find(|r| r.model == "model-a" && r.kind == "compact")
            .expect("compact/model-a group");
        assert_eq!(compact_a.input_tokens, 1000);
        let regular_b = rows
            .iter()
            .find(|r| r.model == "model-b" && r.kind == "regular")
            .expect("regular/model-b group");
        assert_eq!(regular_b.input_tokens, 10);
        assert_eq!(regular_b.output_tokens, 5);
    }

    #[tokio::test]
    async fn usage_seq_is_the_committed_session_entry_seq() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-usage-seq-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        // Commit an assistant entry through the normal located path and
        // recover its ACTUAL session_entries.seq.
        let entry = SessionEntry::Message {
            message: crate::agent::Message::Assistant(crate::agent::AssistantMessage {
                content: Some("hi".into()),
                tool_calls: vec![],
                reasoning: None,
            }),
        };
        let locations = session.append_located(&[entry]).await.unwrap();
        let LocatedKey::Greptime { seq, .. } = locations[0].key else {
            panic!("greptime location must carry a seq");
        };

        // append_usage with the threaded seq: usage_entries.seq must EQUAL
        // the assistant entry's session_entries.seq (exact join), while
        // event_time is an independent event-time clock value.
        session
            .append_usage(
                &wid,
                &sid,
                "model-x",
                "regular",
                Some(seq),
                &crate::agent::Usage {
                    input_tokens: 5,
                    output_tokens: 6,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let rows = session
            .client
            .query(
                "SELECT seq, event_time FROM usage_entries WHERE workspace_id = $1 AND session_id = $2",
                &[&wid, &sid],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        let usage_seq: i64 = rows[0].get("seq");
        assert_eq!(usage_seq, seq, "usage_entries.seq == session_entries.seq");
        let event_time_us = datetime_to_us(rows[0].get::<_, chrono::NaiveDateTime>("event_time"));
        assert!(
            event_time_us != usage_seq,
            "event_time stays an independent clock, not the session seq"
        );

        // None (summarizer / no committed entry): seq falls back to the
        // event-time clock — ordered but NOT joinable to session_entries.
        session
            .append_usage(
                &wid,
                &sid,
                "model-x",
                "summarizer",
                None,
                &crate::agent::Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let rows = session
            .client
            .query(
                "SELECT seq FROM usage_entries WHERE workspace_id = $1 AND session_id = $2 AND kind = 'summarizer'",
                &[&wid, &sid],
            )
            .await
            .unwrap();
        let fallback_seq: i64 = rows[0].get("seq");
        assert!(
            fallback_seq >= 1_700_000_000_000_000,
            "None fallback uses the event-time clock, got {fallback_seq}"
        );
    }

    #[tokio::test]
    async fn append_after_reconnect() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-reconn-{}", crate::session::new_id());
        let entries = test_entries();

        let s1 = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        s1.append(&entries[..3]).await.unwrap();
        drop(s1);

        let s2 = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        s2.append(&entries[3..]).await.unwrap();

        let loaded = s2.load().await.unwrap();
        assert_eq!(loaded.len(), entries.len());
        for (got, want) in loaded.iter().zip(entries.iter()) {
            assert_eq!(got, want);
        }
    }

    #[tokio::test]
    async fn retry_latest_event_time_wins_per_seq() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-retry-{}", crate::session::new_id());
        let entries = test_entries();
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        // First write: seq 0..3
        session.append(&entries[..3]).await.unwrap();
        // Simulate reconnect-retry: same seq range, but new event_times
        session.next_seq.store(0, Ordering::Release);
        session.append(&entries[..3]).await.unwrap();

        // With the new dedup (group by seq, latest event_time wins), each
        // retried seq should keep only the newer write → 3 entries.
        let loaded = session.load().await.unwrap();
        assert_eq!(
            loaded.len(),
            3,
            "retry at different event_times: latest per seq wins (expected 3, got {})",
            loaded.len(),
        );
        // Content assertion: retry wrote the same entries, so all match.
        for (got, want) in loaded.iter().zip(entries[..3].iter()) {
            assert_eq!(got, want, "retry content mismatch");
        }
    }

    #[tokio::test]
    async fn retry_different_payload_rejected_as_conflict() {
        // Same-instance retry with a DIFFERENT payload over an already
        // written seq range is indistinguishable from a concurrent writer
        // (the DB records no writer identity), so the new append-time
        // detection rejects it as a conflict instead of silently
        // overwriting. Read-path latest-event_time folding for divergent
        // payloads is still covered by the dedup_raw_entries unit tests;
        // the write path now refuses to produce such rows.
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-payload-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        // First write: seq 0 with "old_content"
        let old_entry: SessionEntry = Message::User {
            content: "old_content".into(),
            images: vec![],
        }
        .into();
        let new_entry: SessionEntry = Message::User {
            content: "new_content".into(),
            images: vec![],
        }
        .into();

        session
            .append(std::slice::from_ref(&old_entry))
            .await
            .unwrap();
        // Retry same seq 0 with a different payload → conflict error.
        session.next_seq.store(0, Ordering::Release);
        let err = session
            .append(std::slice::from_ref(&new_entry))
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("concurrent write conflict"), "got: {msg}");
        assert!(msg.contains(&sid), "got: {msg}");

        // Nothing was written by the rejected attempt; the original row
        // survives intact.
        let loaded = session.load().await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0], old_entry,
            "rejected retry must not alter the row"
        );
    }

    #[tokio::test]
    async fn append_detects_concurrent_writer() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-conflict-{}", crate::session::new_id());
        let entries = test_entries();

        // Writer A: writes seqs 0..2 (2 entries).
        let writer_a = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        writer_a.append(&entries[..2]).await.unwrap();
        assert_eq!(writer_a.next_seq.load(Ordering::Acquire), 2);
        // A metadata snapshot row (any writer path — create_meta/touch/
        // set_title/set_pinned/backfill — stamps the writer identity), so
        // the conflict error below can name the latest snapshot writer.
        writer_a
            .create_meta(&sid, Some("m"), None, None, None, None)
            .await
            .unwrap();
        drop(writer_a);

        // Writer A2: a second concurrent writer (fresh connect, as a TUI +
        // Web pair or a second process would do) appends seqs 2..4.
        let writer_a2 = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        writer_a2.append(&entries[2..4]).await.unwrap();
        assert_eq!(writer_a2.next_seq.load(Ordering::Acquire), 4);
        drop(writer_a2);

        // Writer B holds a stale next_seq (=2, from before A2's write) and
        // appends DIFFERENT content for seqs 2..4 → must be a conflict.
        let writer_b = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        writer_b.next_seq.store(2, Ordering::Release);
        let foreign: Vec<SessionEntry> = vec![
            Message::User {
                content: "b-writer divergent 2".into(),
                images: vec![],
            }
            .into(),
            Message::User {
                content: "b-writer divergent 3".into(),
                images: vec![],
            }
            .into(),
        ];
        let err = writer_b.append(&foreign).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("concurrent write conflict"), "got: {msg}");
        assert!(msg.contains(&sid), "error must name the session: {msg}");
        assert!(
            msg.contains("max seq") && msg.contains("expected to start"),
            "error must report both the DB max seq and this writer's start seq: {msg}"
        );
        assert!(
            msg.contains("latest metadata writer") && msg.contains(process_identity()),
            "conflict message must name the latest snapshot writer (all snapshots \
             were written by this process in the test): {msg}"
        );

        // The rejected attempt wrote nothing (detection happens before any
        // INSERT), so the DB is untouched.
        let loaded = writer_b.load().await.unwrap();
        assert_eq!(loaded.len(), 4, "rejected attempt must not write rows");

        // Idempotent retry: writer B re-appends A2's EXACT seqs 2..4 →
        // Ok (folded as its own earlier commit), no duplicate rows.
        writer_b.next_seq.store(2, Ordering::Release);
        writer_b.append(&entries[2..4]).await.unwrap();
        assert_eq!(
            writer_b.next_seq.load(Ordering::Acquire),
            4,
            "cursor resumes past the committed rows"
        );
        let loaded = writer_b.load().await.unwrap();
        assert_eq!(loaded.len(), 4, "idempotent retry must not duplicate rows");
        for (got, want) in loaded.iter().zip(entries[..4].iter()) {
            assert_eq!(got, want, "content mismatch after idempotent retry");
        }
    }

    #[tokio::test]
    async fn append_reuses_own_retry() {
        // Same instance: write seqs 0..2, then simulate a
        // "committed but the caller saw an error" retry — the append's
        // next_seq never advanced, so the caller re-appends the exact same
        // entries over the same seq range. The overlap matches this batch
        // exactly, so it is folded as idempotent: Ok, nothing written,
        // read path stays duplicate-free.
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-ownretry-{}", crate::session::new_id());
        let entries = test_entries();
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        // First append: seqs 0..2.
        session.append(&entries[..2]).await.unwrap();
        assert_eq!(session.next_seq.load(Ordering::Acquire), 2);
        // Retry with the cursor the caller still holds after the (errored)
        // first attempt: same seqs, same payloads.
        session.next_seq.store(0, Ordering::Release);
        session.append(&entries[..2]).await.unwrap();
        assert_eq!(
            session.next_seq.load(Ordering::Acquire),
            2,
            "idempotent retry keeps the cursor"
        );

        let loaded = session.load().await.unwrap();
        assert_eq!(
            loaded.len(),
            2,
            "idempotent retry must fold, not duplicate (expected 2, got {})",
            loaded.len(),
        );
        for (got, want) in loaded.iter().zip(entries[..2].iter()) {
            assert_eq!(got, want, "content mismatch after idempotent retry");
        }
    }

    #[tokio::test]
    async fn append_partial_overlap_resumes_remainder() {
        // Partial overlap: a chunked append commits its first chunk (seqs
        // 2..4) but the caller sees an error, so next_seq stays at 2; the
        // retry re-appends the whole 4-entry slice (seqs 2..6). The
        // committed prefix [2,4) matches → folded; the remainder [4,6) is
        // inserted contiguously from db_max+1. No data loss, no duplicates.
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-partial-{}", crate::session::new_id());
        let entries = test_entries();

        // Writer A: seqs 0..2.
        let writer_a = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        writer_a.append(&entries[..2]).await.unwrap();
        drop(writer_a);

        // The "first chunk" of writer B's slice: seqs 2..4 committed by
        // another connection (same content B will retry).
        let writer_b = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        writer_b.next_seq.store(2, Ordering::Release);
        writer_b.append(&entries[2..4]).await.unwrap();
        assert_eq!(writer_b.next_seq.load(Ordering::Acquire), 4);
        // Simulate the error: B still holds the pre-append cursor.
        writer_b.next_seq.store(2, Ordering::Release);

        // Retry the full 4-entry slice: overlap [2,4) matches, remainder
        // [4,6) is inserted at seqs 4,5.
        writer_b.append(&entries[2..6]).await.unwrap();
        assert_eq!(
            writer_b.next_seq.load(Ordering::Acquire),
            6,
            "cursor advances past the remainder"
        );

        let loaded = writer_b.load().await.unwrap();
        assert_eq!(
            loaded.len(),
            6,
            "prefix folded + remainder inserted = 6 entries, no duplicates (got {})",
            loaded.len(),
        );
        for (got, want) in loaded.iter().zip(entries[..6].iter()) {
            assert_eq!(got, want, "content mismatch after partial-overlap retry");
        }
    }

    #[tokio::test]
    async fn toctou_sync_next_seq_from_load() {
        // Simulate: connect → concurrent writer appends → load + advance_next_seq_from_snapshot_len → safe append.
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-toctou-{}", crate::session::new_id());
        let entries = test_entries();

        // Writer A: write seq 0..2 (3 entries)
        let writer_a = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        writer_a.append(&entries[..3]).await.unwrap();
        drop(writer_a);

        // Writer B: connect (reads max_seq=2, sets next_seq=3).
        let mut writer_b = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        // Simulate concurrent writer A' appending seq 3..4 between connect and load.
        let writer_a2 = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        writer_a2.append(&entries[3..5]).await.unwrap();
        drop(writer_a2);

        // Now writer B loads — gets 5 entries (0..4) even though its next_seq
        // is still 3 (stale from connect).  This is the TOCTOU re-read.
        let loaded = writer_b.load_with_seq().await.unwrap();
        assert_eq!(
            loaded.len(),
            5,
            "writer B sees all 5 entries despite stale next_seq"
        );

        // Sync next_seq from the loaded snapshot (what the importer does
        // after TOCTOU check).  Next seq should be 5.
        writer_b
            .advance_next_seq_from_snapshot_len(loaded.len())
            .unwrap();
        assert_eq!(
            writer_b.next_seq.load(Ordering::Acquire),
            5,
            "next_seq updated from loaded snapshot"
        );

        // Now append 2 more entries; they should get seq 5..6, not overlap.
        writer_b.append(&entries[5..7]).await.unwrap();
        let all = writer_b.load().await.unwrap();
        assert_eq!(all.len(), 7, "all 7 entries present, no overlap");
        for (got, want) in all.iter().zip(entries[..7].iter()) {
            assert_eq!(got, want, "content mismatch after TOCTOU-safe append");
        }
    }

    #[tokio::test]
    async fn append_appends_with_contiguous_seq() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-seq-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        let entries = test_entries();

        // Append a slice of 5 entries in one call; verify all 5 recovered.
        session.append(&entries[..5]).await.unwrap();
        let loaded = session.load().await.unwrap();
        assert_eq!(loaded.len(), 5);
        for (got, want) in loaded.iter().zip(entries[..5].iter()) {
            assert_eq!(got, want);
        }

        // Append 2 more; verify all 7 entries recovered.
        session.append(&entries[5..7]).await.unwrap();
        let loaded = session.load().await.unwrap();
        assert_eq!(loaded.len(), 7);
        for (got, want) in loaded.iter().zip(entries[..7].iter()) {
            assert_eq!(got, want);
        }
    }

    #[tokio::test]
    async fn same_session_name_different_workspaces_are_isolated() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid_a = derive_workspace_id(Path::new("/tmp/e-agent-workspace-a"));
        let wid_b = derive_workspace_id(Path::new("/tmp/e-agent-workspace-b"));
        let sid = format!("shared-session-name-{}", crate::session::new_id());

        let sa = GreptimeSession::connect(&conn, &wid_a, &sid).await.unwrap();
        let sb = GreptimeSession::connect(&conn, &wid_b, &sid).await.unwrap();

        let user_msg = Message::User {
            content: "only in workspace A".into(),
            images: vec![],
        };
        sa.append(&[user_msg.clone().into()]).await.unwrap();

        // workspace B should have zero entries despite using the same session name
        let loaded_b = sb.load().await.unwrap();
        assert!(
            loaded_b.is_empty(),
            "workspace B must not see workspace A's entries"
        );

        let loaded_a = sa.load().await.unwrap();
        assert_eq!(loaded_a.len(), 1);
    }

    /// Exact-version semantics through the Greptime backend (requires a live
    /// DB, `GREPTIME_PG`): `append_located` pins (seq, event_time, payload
    /// hash); a same-seq later version (a newer event_time row appended
    /// directly — append mode keeps every physical row) never retargets an
    /// old ref; `read_field` re-checks the pinned event_time + hash.
    #[tokio::test]
    async fn located_append_load_read_exact_version() {
        use crate::output_receipt::{FieldId, issue_legacy_for_test, verify_legacy_for_test};
        use crate::session_store::LocatedKey;

        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-located-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        let entry_a = Message::User {
            content: "version-A".into(),
            images: vec![],
        }
        .into();
        let locations = session
            .append_located(std::slice::from_ref(&entry_a))
            .await
            .unwrap();
        assert_eq!(locations.len(), 1);
        let loc_a = locations[0].clone();
        assert_eq!(loc_a.backend, "greptime");
        assert_eq!(loc_a.session, sid);
        assert_eq!(loc_a.fingerprint.len(), 32);
        assert_eq!(loc_a.entry_hash.len(), 64);
        let LocatedKey::Greptime { seq, event_time_us } = loc_a.key else {
            panic!("greptime location expected");
        };

        // Same-seq later version with different payload (direct insert).
        let entry_b: SessionEntry = Message::User {
            content: "version-B".into(),
            images: vec![],
        }
        .into();
        let payload_b = serde_json::to_string(&entry_b).unwrap();
        session
            .client
            .execute(
                "INSERT INTO session_entries \
                 (workspace_id, session_id, seq, event_time, entry_kind, payload, schema_version, is_error) \
                 VALUES ($1, $2, $3, $4, 'message', $5, 1, false)",
                &[
                    &wid,
                    &sid,
                    &seq,
                    &us_to_datetime(event_time_us + 1),
                    &payload_b,
                ],
            )
            .await
            .unwrap();

        // Read path dedups to the newest (latest event_time wins).
        let loaded = session.load().await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(matches!(
            &loaded[0],
            SessionEntry::Message { message: Message::User { content, .. } } if content == "version-B"
        ));
        // load_located reports the winning physical location.
        let located = session.load_located().await.unwrap();
        assert_eq!(located.len(), 1);
        let new_loc = located[0].0.clone();
        assert!(matches!(
            new_loc.key,
            LocatedKey::Greptime { seq: s, event_time_us: et } if s == seq && et == event_time_us + 1
        ));

        // The OLD receipt still reads the OLD physical row (pinned
        // event_time + hash), never the retargeted newer version.
        let old_ref = issue_legacy_for_test(&loc_a, FieldId::UserContent, "version-A".len());
        let bytes = session
            .read_field(&verify_legacy_for_test(&old_ref).unwrap())
            .await
            .unwrap();
        assert_eq!(bytes, b"version-A");
        // A receipt for the new location reads version B.
        let new_ref = issue_legacy_for_test(&new_loc, FieldId::UserContent, "version-B".len());
        let bytes = session
            .read_field(&verify_legacy_for_test(&new_ref).unwrap())
            .await
            .unwrap();
        assert_eq!(bytes, b"version-B");
        // Tampering the pinned hash fails with integrity.
        let mut tampered = loc_a.clone();
        tampered.entry_hash = "0".repeat(64);
        let bad_ref = issue_legacy_for_test(&tampered, FieldId::UserContent, "version-A".len());
        let err = session
            .read_field(&verify_legacy_for_test(&bad_ref).unwrap())
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("integrity"),
            "hash drift must be integrity: {err:#}"
        );
    }

    /// Full-overlap idempotent retry returns the ACTUAL committed physical
    /// rows (same seq, event_time, hash) — never freshly generated keys.
    #[tokio::test]
    async fn append_full_overlap_returns_actual_committed_locations() {
        use crate::output_receipt::{FieldId, issue_legacy_for_test, verify_legacy_for_test};
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-fullov-{}", crate::session::new_id());
        let entries: Vec<SessionEntry> = vec![
            Message::User {
                content: "first".into(),
                images: vec![],
            }
            .into(),
            Message::User {
                content: "second".into(),
                images: vec![],
            }
            .into(),
        ];

        let writer_a = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        let locs_a = writer_a.append_located(&entries).await.unwrap();
        assert_eq!(locs_a.len(), 2);
        drop(writer_a);

        let writer_b = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        writer_b.next_seq.store(0, Ordering::Release); // committed-then-errored retry reuses seqs
        let locs_b = writer_b.append_located(&entries).await.unwrap();

        assert_eq!(locs_a, locs_b, "retry must return the committed rows");

        // Every returned location resolves through read_field.
        for (i, loc) in locs_b.iter().enumerate() {
            let content = &["first", "second"][i];
            let receipt = issue_legacy_for_test(loc, FieldId::UserContent, content.len());
            let bytes = writer_b
                .read_field(&verify_legacy_for_test(&receipt).unwrap())
                .await
                .unwrap();
            assert_eq!(bytes, content.as_bytes(), "location must resolve");
        }
    }

    /// Partial-overlap retry returns the ACTUAL committed rows for the
    /// accepted prefix and the exact inserted rows for the suffix.
    #[tokio::test]
    async fn append_partial_overlap_returns_actual_locations() {
        use crate::output_receipt::{FieldId, issue_legacy_for_test, verify_legacy_for_test};
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-partov-{}", crate::session::new_id());
        let entries: Vec<SessionEntry> = vec![
            Message::User {
                content: "a".into(),
                images: vec![],
            }
            .into(),
            Message::User {
                content: "b".into(),
                images: vec![],
            }
            .into(),
            Message::User {
                content: "c".into(),
                images: vec![],
            }
            .into(),
            Message::User {
                content: "d".into(),
                images: vec![],
            }
            .into(),
        ];

        let writer_a = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        writer_a.append(&entries[..2]).await.unwrap();
        drop(writer_a);

        let writer_b = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        let locs_first = writer_b.append_located(&entries[2..4]).await.unwrap();
        writer_b.next_seq.store(2, Ordering::Release); // errored retry
        let mut more: Vec<SessionEntry> = entries[2..].to_vec();
        more.push(
            Message::User {
                content: "e".into(),
                images: vec![],
            }
            .into(),
        );
        let locs_retry = writer_b.append_located(&more).await.unwrap();
        assert_eq!(locs_retry.len(), 3);
        // Prefix (seqs 2,3): the ACTUAL committed rows — identical to B's
        // own first commit's locations.
        assert_eq!(
            &locs_retry[..2],
            &locs_first[..],
            "accepted prefix must re-read the actual committed rows"
        );
        // Suffix (seq 4): contiguous, resolves via read_field.
        let contents = ["c", "d", "e"];
        for (i, loc) in locs_retry.iter().enumerate() {
            let receipt = issue_legacy_for_test(loc, FieldId::UserContent, contents[i].len());
            let bytes = writer_b
                .read_field(&verify_legacy_for_test(&receipt).unwrap())
                .await
                .unwrap();
            assert_eq!(bytes, contents[i].as_bytes(), "location must resolve");
        }
    }

    /// The post-insert fail-closed detection (finding: Greptime has no
    /// transactions/leases, so after committing we re-read the written
    /// range and require OUR rows to be the physical winners) — a foreign
    /// row superseding ours at the same seq is detected by the exact
    /// comparison the append runs after every insert.
    #[tokio::test]
    async fn post_insert_fail_closed_detects_superseded_foreign_row() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-failclosed-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        let entry: SessionEntry = Message::User {
            content: "mine".into(),
            images: vec![],
        }
        .into();
        // Normal append: the post-insert check runs and passes.
        let locs = session
            .append_located(std::slice::from_ref(&entry))
            .await
            .unwrap();
        assert_eq!(locs.len(), 1);
        let crate::session_store::LocatedKey::Greptime { seq, event_time_us } = locs[0].key else {
            panic!("greptime location");
        };

        // A foreign writer races the window and commits a LATER row at the
        // same seq with a DIFFERENT payload — exactly the committed
        // ambiguity the post-insert verification must fail closed on.
        let foreign: SessionEntry = Message::User {
            content: "foreign".into(),
            images: vec![],
        }
        .into();
        let payload_foreign = serde_json::to_string(&foreign).unwrap();
        session
            .client
            .execute(
                "INSERT INTO session_entries \
                 (workspace_id, session_id, seq, event_time, entry_kind, payload, schema_version, is_error) \
                 VALUES ($1, $2, $3, $4, 'message', $5, 1, false)",
                &[
                    &wid,
                    &sid,
                    &seq,
                    &us_to_datetime(event_time_us + 1),
                    &payload_foreign,
                ],
            )
            .await
            .unwrap();

        // The exact post-insert comparison (overlap_conflict_seq over the
        // written range) must report the seq: our row is no longer the
        // winner. append_located would fail closed with a conflict instead
        // of advertising a location the read path would not pick.
        let prepped = vec![(
            seq,
            us_to_datetime(event_time_us),
            "message".to_owned(),
            serde_json::to_string(&entry).unwrap(),
            false,
        )];
        let conflict = session
            .overlap_conflict_seq(&prepped, seq, seq)
            .await
            .unwrap();
        assert_eq!(conflict, Some(seq), "superseded row must fail closed");
        // The logical read path agrees: the foreign row wins.
        let loaded = session.load().await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(matches!(
            &loaded[0],
            SessionEntry::Message { message: Message::User { content, .. } } if content == "foreign"
        ));
    }

    /// Exact-key read with multiple physical rows at the pinned key:
    /// identical duplicates fold and resolve; a DIVERGENT duplicate fails
    /// closed with integrity even though the signed row exists (the
    /// arbitrary-row `query_opt` behavior would be ambiguous).
    #[tokio::test]
    async fn read_field_exact_key_folds_identical_and_rejects_divergent_duplicates() {
        use crate::output_receipt::{FieldId, issue_legacy_for_test, verify_legacy_for_test};
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-exactdup-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        let entry: SessionEntry = Message::User {
            content: "dup".into(),
            images: vec![],
        }
        .into();
        let locs = session
            .append_located(std::slice::from_ref(&entry))
            .await
            .unwrap();
        let loc = locs[0].clone();
        let crate::session_store::LocatedKey::Greptime { seq, event_time_us } = loc.key else {
            panic!("greptime location");
        };
        let payload = serde_json::to_string(&entry).unwrap();

        // Identical duplicate at the exact same key (idempotent retry at
        // the same microsecond): folds, receipt resolves.
        session
            .client
            .execute(
                "INSERT INTO session_entries \
                 (workspace_id, session_id, seq, event_time, entry_kind, payload, schema_version, is_error) \
                 VALUES ($1, $2, $3, $4, 'message', $5, 1, false)",
                &[&wid, &sid, &seq, &us_to_datetime(event_time_us), &payload],
            )
            .await
            .unwrap();
        let receipt = issue_legacy_for_test(&loc, FieldId::UserContent, "dup".len());
        let bytes = session
            .read_field(&verify_legacy_for_test(&receipt).unwrap())
            .await
            .unwrap();
        assert_eq!(bytes, b"dup");

        // A DIVERGENT duplicate at the exact same key: the signed row
        // exists, but the key is ambiguous — fail closed.
        let foreign: SessionEntry = Message::User {
            content: "divergent".into(),
            images: vec![],
        }
        .into();
        session
            .client
            .execute(
                "INSERT INTO session_entries \
                 (workspace_id, session_id, seq, event_time, entry_kind, payload, schema_version, is_error) \
                 VALUES ($1, $2, $3, $4, 'message', $5, 1, false)",
                &[
                    &wid,
                    &sid,
                    &seq,
                    &us_to_datetime(event_time_us),
                    &serde_json::to_string(&foreign).unwrap(),
                ],
            )
            .await
            .unwrap();
        let err = session
            .read_field(&verify_legacy_for_test(&receipt).unwrap())
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("divergent"),
            "divergent duplicates must fail closed: {err:#}"
        );
    }

    /// The located loader hashes the EXACT raw stored payload bytes (the
    /// same bytes `read_field` re-hashes): a whitespace/key-order variant
    /// stored in the DB must still produce a receipt that resolves.
    #[tokio::test]
    async fn load_located_hashes_exact_raw_payload_bytes() {
        use crate::output_receipt::{FieldId, issue_legacy_for_test, verify_legacy_for_test};
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-rawhash-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        // Externally-tagged Message inside an internally-tagged entry:
        // reordered keys + whitespace, same entry, different raw bytes.
        let raw = r#"{ "message": { "User": { "images": [ ], "content": "variant" } }, "type": "message" }"#;
        session
            .client
            .execute(
                "INSERT INTO session_entries \
                 (workspace_id, session_id, seq, event_time, entry_kind, payload, schema_version, is_error) \
                 VALUES ($1, $2, 0, $3, 'message', $4, 1, false)",
                &[
                    &wid,
                    &sid,
                    &us_to_datetime(next_event_time_us()),
                    &raw,
                ],
            )
            .await
            .unwrap();

        let located = session.load_located().await.unwrap();
        assert_eq!(located.len(), 1);
        let (loc, entry) = &located[0];
        assert!(
            matches!(
                entry,
                SessionEntry::Message { message: Message::User { content, .. } } if content == "variant"
            ),
            "variant must deserialize to the same entry"
        );
        assert_eq!(
            loc.entry_hash,
            crate::session_store::entry_payload_hash(raw),
            "loader must hash the exact raw stored payload"
        );
        // A receipt issued against that location resolves (under the old
        // re-serialization hashing it would fail integrity).
        let receipt = issue_legacy_for_test(loc, FieldId::UserContent, "variant".len());
        let bytes = session
            .read_field(&verify_legacy_for_test(&receipt).unwrap())
            .await
            .unwrap();
        assert_eq!(bytes, b"variant");
    }

    #[tokio::test]
    async fn load_head_and_load_older_segmented() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-seg-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        let early: Vec<SessionEntry> = vec![
            Message::User {
                content: "early 1".into(),
                images: vec![],
            }
            .into(),
            Message::User {
                content: "early 2".into(),
                images: vec![],
            }
            .into(),
        ];
        let comp1 = SessionEntry::Compaction {
            summary: "compaction 1".into(),
            retained: vec![Message::User {
                content: "retained 1".into(),
                images: vec![],
            }],
            current_prompt_at: None,
            no_current_prompt: false,
        };
        let middle: Vec<SessionEntry> = vec![
            Message::User {
                content: "middle 1".into(),
                images: vec![],
            }
            .into(),
            Message::User {
                content: "middle 2".into(),
                images: vec![],
            }
            .into(),
        ];
        let comp2 = SessionEntry::Compaction {
            summary: "compaction 2".into(),
            retained: vec![Message::User {
                content: "retained 2".into(),
                images: vec![],
            }],
            current_prompt_at: None,
            no_current_prompt: false,
        };
        let latest: Vec<SessionEntry> = vec![
            Message::User {
                content: "latest 1".into(),
                images: vec![],
            }
            .into(),
            Message::User {
                content: "latest 2".into(),
                images: vec![],
            }
            .into(),
        ];

        let mut all = Vec::new();
        all.extend(early.iter().cloned());
        all.push(comp1.clone());
        all.extend(middle.iter().cloned());
        all.push(comp2.clone());
        all.extend(latest.iter().cloned());
        session.append(&all).await.unwrap();

        // Append order = contiguous seqs: early=0,1, comp1=2, middle=3,4,
        // comp2=5, latest=6,7.
        let comp1_seq = 2i64;
        let comp2_seq = 5i64;

        // Head segment: [comp2, ∞) = comp2 + latest only.
        let head = session.load_head().await.unwrap();
        assert_eq!(
            head.len(),
            1 + latest.len(),
            "head must contain comp2 + latest, got {} entries",
            head.len(),
        );
        assert_eq!(head[0], comp2, "head opens with the last compaction");
        for (got, want) in head.iter().skip(1).zip(latest.iter()) {
            assert_eq!(got, want, "head tail mismatch");
        }

        // Middle segment: load_older(comp2_seq, None) = [comp1, comp2) =
        // comp1 + middle, cursor = comp1_seq.
        let (seg, cursor) = session.load_older(comp2_seq, None).await.unwrap();
        assert_eq!(
            cursor,
            Some(comp1_seq),
            "middle segment cursor must be comp1's seq"
        );
        assert_eq!(
            seg.len(),
            1 + middle.len(),
            "middle segment must contain comp1 + middle, got {} entries",
            seg.len(),
        );
        assert_eq!(seg[0], comp1, "middle segment opens with comp1");
        for (got, want) in seg.iter().skip(1).zip(middle.iter()) {
            assert_eq!(got, want, "middle segment tail mismatch");
        }

        // Oldest segment: load_older(comp1_seq, None) = [0, comp1) = early,
        // and cursor None (nothing older).
        let (oldest, cursor) = session.load_older(comp1_seq, None).await.unwrap();
        assert_eq!(cursor, None, "oldest segment cursor must be None");
        assert_eq!(
            oldest, early,
            "oldest segment must contain only early entries"
        );

        // Exactly-once coverage across the chain: head + middle + oldest
        // accounts for every appended entry, with each compaction row in
        // exactly one segment.
        let total: usize = head.len() + seg.len() + oldest.len();
        assert_eq!(total, all.len(), "segments must cover the whole session");
    }

    /// Intra-segment paging: with `limit` set, `load_older` returns only
    /// the `limit` entries closest to `before_seq` (the segment tail) and a
    /// cursor pointing at the page's oldest entry, so the caller pages
    /// backward through a long segment in fixed-size chunks. The cursor
    /// stays `Some` across a middle segment's compaction boundary (the next
    /// call crosses into the older segment) and becomes `None` only at the
    /// true start of the session; every entry is returned exactly once.
    #[tokio::test]
    async fn load_older_pages_within_segment() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-paged-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        let early: Vec<SessionEntry> = vec![
            Message::User {
                content: "early 1".into(),
                images: vec![],
            }
            .into(),
            Message::User {
                content: "early 2".into(),
                images: vec![],
            }
            .into(),
        ];
        let comp1 = SessionEntry::Compaction {
            summary: "compaction 1".into(),
            retained: vec![],
            current_prompt_at: None,
            no_current_prompt: false,
        };
        // 7 middle entries: enough that one segment needs several 2-entry
        // pages.
        let middle: Vec<SessionEntry> = (0..7)
            .map(|i| {
                Message::User {
                    content: format!("middle {i}"),
                    images: vec![],
                }
                .into()
            })
            .collect();
        let comp2 = SessionEntry::Compaction {
            summary: "compaction 2".into(),
            retained: vec![],
            current_prompt_at: None,
            no_current_prompt: false,
        };
        let latest: Vec<SessionEntry> = vec![
            Message::User {
                content: "latest 1".into(),
                images: vec![],
            }
            .into(),
            Message::User {
                content: "latest 2".into(),
                images: vec![],
            }
            .into(),
        ];

        let mut all = Vec::new();
        all.extend(early.iter().cloned());
        all.push(comp1.clone());
        all.extend(middle.iter().cloned());
        all.push(comp2.clone());
        all.extend(latest.iter().cloned());
        session.append(&all).await.unwrap();

        // Append order = contiguous seqs: early=0,1, comp1=2, middle=3..9,
        // comp2=10, latest=11,12. So middle[i] sits at seq 3+i, comp1 at 2,
        // comp2 at 10.
        let comp1_seq = 2i64;
        let comp2_seq = 10i64;

        // Page 1 of the middle segment [2,10): the 2 entries closest to
        // comp2 (tail) = seqs 9,8 → ascending [middle 5, middle 6];
        // cursor = oldest page seq = 8 (still inside the segment).
        let (page1, cursor1) = session.load_older(comp2_seq, Some(2)).await.unwrap();
        assert_eq!(
            page1,
            vec![middle[5].clone(), middle[6].clone()],
            "first page must be the segment tail (newest entries), ascending"
        );
        assert_eq!(
            cursor1,
            Some(8),
            "cursor must point at the page's oldest entry"
        );

        // Page 2: [2,8) tail = seqs 7,6 → [middle 3, middle 4]; cursor 6.
        let (page2, cursor2) = session.load_older(cursor1.unwrap(), Some(2)).await.unwrap();
        assert_eq!(
            page2,
            vec![middle[3].clone(), middle[4].clone()],
            "second page must be the next older 2 entries"
        );
        assert_eq!(cursor2, Some(6));

        // Page 3: [2,6) tail = seqs 5,4 → [middle 1, middle 2]; cursor 4.
        let (page3, cursor3) = session.load_older(cursor2.unwrap(), Some(2)).await.unwrap();
        assert_eq!(
            page3,
            vec![middle[1].clone(), middle[2].clone()],
            "third page must be the next older 2 entries"
        );
        assert_eq!(cursor3, Some(4));

        // Page 4: [2,4) = seqs 3,2 → [comp1, middle 0]; the page reaches
        // the segment's opening compaction row (seq 2 = prev_comp), so the
        // cursor stays Some(2) and the next call crosses into the older
        // segment.
        let (page4, cursor4) = session.load_older(cursor3.unwrap(), Some(2)).await.unwrap();
        assert_eq!(
            page4,
            vec![comp1.clone(), middle[0].clone()],
            "last page of the middle segment must reach its compaction row"
        );
        assert_eq!(
            cursor4,
            Some(comp1_seq),
            "cursor must stay Some at the segment's compaction boundary"
        );

        // Page 5 (oldest segment [0,2) = early): 2 entries ≤ limit → whole
        // segment, cursor None — nothing older exists.
        let (page5, cursor5) = session.load_older(cursor4.unwrap(), Some(2)).await.unwrap();
        assert_eq!(page5, early, "oldest segment must come back whole");
        assert_eq!(cursor5, None, "cursor must be None at the true start");

        // Exactly-once coverage across the paged chain: every appended
        // entry appears in exactly one page (head + 5 pages).
        let head = session.load_head().await.unwrap();
        let total: usize =
            head.len() + page1.len() + page2.len() + page3.len() + page4.len() + page5.len();
        assert_eq!(total, all.len(), "paged chain must cover the whole session");
    }

    /// A paged query is limited by physical rows. Rust deduplicates only the
    /// rows returned by that page, so same-seq retries can make the logical
    /// page smaller than its physical-row limit. The cursor still advances
    /// from the oldest seq present in the physical page.
    #[tokio::test]
    async fn load_older_physical_page_order_and_cursor() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-physical-page-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        let user = |i: u32| SessionEntry::Message {
            message: Message::User {
                content: format!("m{i}"),
                images: vec![],
            },
        };
        let plain: Vec<SessionEntry> = (0..6).map(user).collect();
        session.append(&plain).await.unwrap();

        // The page's three physical rows are seq 5 twice and seq 4 once.
        // Rust deduplication leaves two logical entries, rather than filling
        // the page to three distinct seqs.
        let prepped: Vec<(i64, chrono::NaiveDateTime, String, String, bool)> = [4i64, 5i64]
            .into_iter()
            .map(|seq| {
                let entry = user(seq as u32);
                (
                    seq,
                    us_to_datetime(next_event_time_us()),
                    entry_kind(&entry).to_string(),
                    serde_json::to_string(&entry).unwrap(),
                    is_error(&entry),
                )
            })
            .collect();
        session.insert_prepped(&prepped).await.unwrap();

        let (page1, cursor1) = session.load_older(i64::MAX, Some(3)).await.unwrap();
        assert_eq!(page1, vec![user(4), user(5)]);
        assert_eq!(cursor1, Some(4));

        // Cursor progression uses seq bounds, and ordinary unique-seq rows
        // continue in direct descending SQL order (returned ascending).
        let (page2, cursor2) = session.load_older(cursor1.unwrap(), Some(3)).await.unwrap();
        assert_eq!(page2, vec![user(1), user(2), user(3)]);
        assert_eq!(cursor2, Some(1));

        let (page3, cursor3) = session.load_older(cursor2.unwrap(), Some(3)).await.unwrap();
        assert_eq!(page3, vec![user(0)]);
        assert_eq!(cursor3, None);
    }

    /// Store-level `load_head_page` (the `GET /history` initial-render
    /// path): the bounded head page must never strand the truncated part
    /// of the head segment — the returned cursor feeds straight back into
    /// `load_older` and the whole chain covers every entry exactly once.
    #[tokio::test]
    async fn store_load_head_page_pages_without_losing_segments() {
        use crate::config::SessionBackend;
        use crate::session_store::SessionStore;
        use std::path::Path;

        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let root = Path::new("/tmp/e-agent-test");

        let user = |i: u32| SessionEntry::Message {
            message: Message::User {
                content: format!("m{i}"),
                images: vec![],
            },
        };
        let comp = |summary: &str| SessionEntry::Compaction {
            summary: summary.into(),
            retained: vec![],
            current_prompt_at: None,
            no_current_prompt: false,
        };

        // ---- No compaction: whole session is one head segment. No limit
        // → whole session + None cursor; bounded page → newest `limit`
        // entries with a cursor that pages through the rest (nothing
        // stranded).
        let session_a = format!("test-gt-head-a-{}", crate::session::new_id());
        let store_a = SessionStore::connect(
            &SessionBackend::Greptime { conn: conn.clone() },
            root,
            &session_a,
        )
        .await
        .unwrap();
        let plain = vec![user(1), user(2), user(3)];
        store_a.append(root, &session_a, &plain).await.unwrap();
        let (page, cursor) = store_a
            .load_head_page(root, &session_a, None)
            .await
            .expect("load_head_page without limit");
        assert_eq!(page, plain, "no-compaction session returned whole");
        assert_eq!(cursor, None, "no compaction → no older cursor");
        let (page, cursor) = store_a
            .load_head_page(root, &session_a, Some(2))
            .await
            .expect("load_head_page with limit");
        assert_eq!(page, vec![user(2), user(3)], "newest limit entries");
        assert_eq!(cursor, Some(1), "cursor = oldest seq of the page");
        let (rest, cursor) = store_a
            .load_older(root, &session_a, cursor.unwrap(), Some(2))
            .await
            .expect("load_older");
        assert_eq!(rest, vec![user(1)], "remaining entry reachable");
        assert_eq!(cursor, None, "seq 0 page → nothing older");

        // ---- Compaction with head ≤ limit: whole head segment + cursor
        // = the opening compaction's seq.
        // seqs: 0,1 early; 2 comp1; 3,4 middle; 5 comp2; 6,7 latest.
        let session_b = format!("test-gt-head-b-{}", crate::session::new_id());
        let store_b = SessionStore::connect(
            &SessionBackend::Greptime { conn: conn.clone() },
            root,
            &session_b,
        )
        .await
        .unwrap();
        let mut all = vec![user(1), user(2)];
        all.push(comp("c1"));
        all.extend([user(3), user(4)]);
        all.push(comp("c2"));
        all.extend([user(5), user(6)]);
        store_b.append(root, &session_b, &all).await.unwrap();
        let (page, cursor) = store_b
            .load_head_page(root, &session_b, Some(3))
            .await
            .expect("load_head_page with limit");
        assert_eq!(
            page,
            vec![comp("c2"), user(5), user(6)],
            "head ≤ limit → whole head segment"
        );
        assert_eq!(cursor, Some(5), "cursor = opening compaction seq");

        // ---- Compaction with head > limit: newest `limit` entries, cursor
        // = oldest seq of the page; paging back with that cursor reaches
        // the cut-off part, then crosses compaction boundaries — every
        // entry is covered exactly once.
        // seqs: 0,1 early; 2 comp1; 3,4 middle; 5 comp2; 6..9 latest.
        let session_c = format!("test-gt-head-c-{}", crate::session::new_id());
        let store_c = SessionStore::connect(
            &SessionBackend::Greptime { conn: conn.clone() },
            root,
            &session_c,
        )
        .await
        .unwrap();
        let mut all = vec![user(1), user(2)];
        all.push(comp("c1"));
        all.extend([user(3), user(4)]);
        all.push(comp("c2"));
        all.extend([user(5), user(6), user(7), user(8)]);
        store_c.append(root, &session_c, &all).await.unwrap();

        let mut paged: Vec<SessionEntry> = Vec::new();
        let mut cursor: Option<i64> = Some(i64::MAX); // head open sentinel
        loop {
            let (entries, next) = match cursor {
                Some(i64::MAX) => store_c
                    .load_head_page(root, &session_c, Some(2))
                    .await
                    .expect("head page"),
                Some(before) => store_c
                    .load_older(root, &session_c, before, Some(2))
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

        // Spot-check the boundary page explicitly: the first page is the
        // newest 2 of the head, and its cursor pages into the cut-off head
        // part (not past the head into older segments).
        let (page, cursor) = store_c
            .load_head_page(root, &session_c, Some(2))
            .await
            .expect("head page C");
        assert_eq!(page, vec![user(7), user(8)], "newest 2 of head");
        let (next, _) = store_c
            .load_older(root, &session_c, cursor.unwrap(), Some(2))
            .await
            .expect("older page C");
        assert_eq!(
            next,
            vec![user(5), user(6)],
            "cut-off head part reachable via cursor"
        );
    }

    #[tokio::test]
    async fn load_oldest_and_load_newer_segmented() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-newer-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        let early: Vec<SessionEntry> = vec![
            Message::User {
                content: "early 1".into(),
                images: vec![],
            }
            .into(),
            Message::User {
                content: "early 2".into(),
                images: vec![],
            }
            .into(),
        ];
        let comp1 = SessionEntry::Compaction {
            summary: "compaction 1".into(),
            retained: vec![Message::User {
                content: "retained 1".into(),
                images: vec![],
            }],
            current_prompt_at: None,
            no_current_prompt: false,
        };
        let middle: Vec<SessionEntry> = vec![
            Message::User {
                content: "middle 1".into(),
                images: vec![],
            }
            .into(),
            Message::User {
                content: "middle 2".into(),
                images: vec![],
            }
            .into(),
        ];
        let comp2 = SessionEntry::Compaction {
            summary: "compaction 2".into(),
            retained: vec![Message::User {
                content: "retained 2".into(),
                images: vec![],
            }],
            current_prompt_at: None,
            no_current_prompt: false,
        };
        let latest: Vec<SessionEntry> = vec![
            Message::User {
                content: "latest 1".into(),
                images: vec![],
            }
            .into(),
            Message::User {
                content: "latest 2".into(),
                images: vec![],
            }
            .into(),
        ];

        let mut all = Vec::new();
        all.extend(early.iter().cloned());
        all.push(comp1.clone());
        all.extend(middle.iter().cloned());
        all.push(comp2.clone());
        all.extend(latest.iter().cloned());
        session.append(&all).await.unwrap();

        // Append order = contiguous seqs: early=0,1, comp1=2, middle=3,4,
        // comp2=5, latest=6,7.
        let comp1_seq = 2i64;
        let comp2_seq = 5i64;

        // Oldest segment: load_oldest() = [0, comp1) = early, cursor =
        // comp1 (the load_newer starting point).
        let (oldest, cursor) = session.load_oldest().await.unwrap();
        assert_eq!(
            cursor,
            Some(comp1_seq),
            "oldest cursor must be the first compaction seq"
        );
        assert_eq!(
            oldest, early,
            "oldest segment must contain only the early entries"
        );

        // Middle segment: load_newer(comp1) = [comp1, comp2) = comp1 +
        // middle, cursor = comp2.
        let (middle_seg, cursor) = session.load_newer(comp1_seq).await.unwrap();
        assert_eq!(
            cursor,
            Some(comp2_seq),
            "middle segment cursor must be the next compaction seq"
        );
        assert_eq!(
            middle_seg.len(),
            1 + middle.len(),
            "middle segment must contain comp1 + middle, got {} entries",
            middle_seg.len(),
        );
        assert_eq!(middle_seg[0], comp1, "middle segment opens with comp1");
        for (got, want) in middle_seg.iter().skip(1).zip(middle.iter()) {
            assert_eq!(got, want, "middle segment tail mismatch");
        }

        // Head boundary: load_newer(comp2) must return nothing — the head
        // segment [comp2, ∞) is already loaded by load_head and must not
        // be duplicated.
        let (after, cursor) = session.load_newer(comp2_seq).await.unwrap();
        assert_eq!(cursor, None, "no compaction after comp2 → cursor None");
        assert!(
            after.is_empty(),
            "load_newer must never return the head segment"
        );

        // Exactly-once coverage across the chain: oldest + middle + head
        // accounts for every appended entry.
        let head = session.load_head().await.unwrap();
        let total: usize = oldest.len() + middle_seg.len() + head.len();
        assert_eq!(total, all.len(), "segments must cover the whole session");

        // A session with no compaction at all: load_oldest reports nothing
        // older (the head segment already covers everything).
        let sid_none = format!("test-gt-nocomp-{}", crate::session::new_id());
        let no_comp = GreptimeSession::connect(&conn, &wid, &sid_none)
            .await
            .unwrap();
        no_comp.append(&early).await.unwrap();
        let (entries, cursor) = no_comp.load_oldest().await.unwrap();
        assert!(entries.is_empty(), "no compaction → nothing older to load");
        assert_eq!(cursor, None, "no compaction → cursor None");
        // And load_newer from a cursor that cannot exist in that session
        // still behaves (no compaction after any seq → None).
        let (entries, cursor) = no_comp.load_newer(0).await.unwrap();
        assert!(entries.is_empty());
        assert_eq!(cursor, None);
    }

    // ------------------------------------------------------------------
    // dedup_raw_entries — pure unit tests (no live DB)
    // ------------------------------------------------------------------

    fn et(sec: i64) -> chrono::NaiveDateTime {
        chrono::DateTime::from_timestamp(sec, 0)
            .unwrap()
            .naive_utc()
    }

    fn msg_entry(content: &str) -> String {
        serde_json::to_string(&SessionEntry::from(Message::User {
            content: content.to_owned(),
            images: vec![],
        }))
        .unwrap()
    }

    // --- older overwritten ---

    #[test]
    fn dedup_older_overwritten_different_payload() {
        // Same seq=5, older event_time with payload "old" gets overwritten
        // by newer event_time with payload "new" → only "new" survives.
        let raw = vec![
            (5i64, et(2000), msg_entry("new")),
            (5i64, et(1000), msg_entry("old")),
        ];
        let result =
            dedup_raw_entries(&raw, "test-session", "test-workspace", "event_time").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, 5);
        let payload = serde_json::to_string(&result[0].1).unwrap();
        assert_eq!(payload, msg_entry("new"));
    }

    #[test]
    fn dedup_older_overwritten_three_versions() {
        // Three event_times for same seq; only the latest survives.
        let raw = vec![
            (1i64, et(3000), msg_entry("v3")),
            (1i64, et(1000), msg_entry("v1")),
            (1i64, et(2000), msg_entry("v2")),
        ];
        let result =
            dedup_raw_entries(&raw, "test-session", "test-workspace", "event_time").unwrap();
        assert_eq!(result.len(), 1);
        let payload = serde_json::to_string(&result[0].1).unwrap();
        assert_eq!(payload, msg_entry("v3"));
    }

    #[test]
    fn dedup_older_overwritten_identical_payload() {
        // Same seq, older and newer but same payload → only one entry (no conflict).
        let raw = vec![
            (5i64, et(2000), msg_entry("hello")),
            (5i64, et(1000), msg_entry("hello")),
        ];
        let result =
            dedup_raw_entries(&raw, "test-session", "test-workspace", "event_time").unwrap();
        assert_eq!(result.len(), 1);
    }

    // --- latest tie identical folded ---

    #[test]
    fn dedup_latest_tie_identical_folded() {
        // Two rows with same (seq=3, event_time) and same payload → folded
        let raw = vec![
            (3i64, et(500), msg_entry("hello")),
            (3i64, et(500), msg_entry("hello")),
        ];
        let result =
            dedup_raw_entries(&raw, "test-session", "test-workspace", "event_time").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, 3);
    }

    #[test]
    fn dedup_latest_tie_three_identical_folded() {
        // Three rows with same (seq=7, event_time, payload) → folded
        let raw = vec![
            (7i64, et(999), msg_entry("triple")),
            (7i64, et(999), msg_entry("triple")),
            (7i64, et(999), msg_entry("triple")),
        ];
        let result =
            dedup_raw_entries(&raw, "test-session", "test-workspace", "event_time").unwrap();
        assert_eq!(result.len(), 1);
    }

    // --- latest tie divergent rejected ---

    #[test]
    fn dedup_latest_tie_divergent_rejected() {
        // Same (seq=3, event_time=500) but different payloads → conflict error
        let raw = vec![
            (3i64, et(500), msg_entry("hello")),
            (3i64, et(500), msg_entry("world")),
        ];
        let err =
            dedup_raw_entries(&raw, "test-session", "test-workspace", "event_time").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("divergent physical duplicates"), "got: {msg}");
        assert!(msg.contains("seq 3"), "got: {msg}");
    }

    #[test]
    fn dedup_latest_tie_divergent_with_older_rows() {
        // seq=5 has two rows at max event_time (divergent), plus an older row.
        // The older row is discarded but the conflict at max ET is still detected.
        let raw = vec![
            (5i64, et(2000), msg_entry("new_a")),
            (5i64, et(2000), msg_entry("new_b")), // divergent!
            (5i64, et(1000), msg_entry("old")),
        ];
        let err =
            dedup_raw_entries(&raw, "test-session", "test-workspace", "event_time").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("divergent physical duplicates"), "got: {msg}");
        assert!(msg.contains("seq 5"), "got: {msg}");
    }

    // --- canonical output order ---

    #[test]
    fn dedup_output_winning_event_time_precedes_seq() {
        // Winning event_time defines conversation order even when it conflicts
        // with seq order; seq remains attached as identity metadata.
        let raw = vec![
            (10i64, et(3000), msg_entry("ten")),
            (5i64, et(1000), msg_entry("five")),
            (1i64, et(2000), msg_entry("one")),
        ];
        let result =
            dedup_raw_entries(&raw, "test-session", "test-workspace", "event_time").unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].0, 5, "earliest winning event_time first");
        assert_eq!(result[1].0, 1, "middle winning event_time second");
        assert_eq!(result[2].0, 10, "latest winning event_time last");
    }

    #[test]
    fn dedup_output_seq_tiebreaks_same_event_time() {
        // seq is the deterministic tie-breaker for equal winning event_time.
        let raw = vec![
            (20i64, et(1000), msg_entry("seq20")),
            (5i64, et(1000), msg_entry("seq5")),
        ];
        let result =
            dedup_raw_entries(&raw, "test-session", "test-workspace", "event_time").unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0, 5, "lower seq first");
        assert_eq!(result[1].0, 20, "higher seq second");
    }

    #[test]
    fn dedup_versions_use_winning_time_for_canonical_order() {
        // The older physical version must neither survive nor determine order.
        let raw = vec![
            (0i64, et(100), msg_entry("seq0-old")),
            (0i64, et(400), msg_entry("seq0-new")),
            (0i64, et(400), msg_entry("seq0-new")), // winning tie folds
            (1i64, et(300), msg_entry("seq1")),
        ];
        let result =
            dedup_raw_entries(&raw, "test-session", "test-workspace", "event_time").unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0, 1);
        assert_eq!(result[1].0, 0);
        assert_eq!(
            serde_json::to_string(&result[1].1).unwrap(),
            msg_entry("seq0-new")
        );
    }

    // --- edge cases ---

    #[test]
    fn dedup_empty_input() {
        let result =
            dedup_raw_entries(&[], "test-session", "test-workspace", "event_time").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn dedup_multi_seq_mixed_older_latest() {
        // Mix of seqs with various age relationships.
        let raw = vec![
            (1i64, et(100), msg_entry("seq1-old")),
            (1i64, et(300), msg_entry("seq1-latest")),
            (2i64, et(200), msg_entry("seq2-only")),
            (3i64, et(150), msg_entry("seq3-only")),
            (3i64, et(150), msg_entry("seq3-only")), // folded: same et, same payload
        ];
        let result =
            dedup_raw_entries(&raw, "test-session", "test-workspace", "event_time").unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].0, 3);
        assert_eq!(result[1].0, 2);
        assert_eq!(result[2].0, 1);
        assert_eq!(
            serde_json::to_string(&result[2].1).unwrap(),
            msg_entry("seq1-latest")
        );
    }

    #[test]
    fn workspace_id_is_stable() {
        let a = derive_workspace_id(Path::new("/tmp/ws"));
        let b = derive_workspace_id(Path::new("/tmp/ws"));
        assert_eq!(a, b);
    }

    /// The writer identity is computed once per process (OnceLock) and has
    /// the `pid@hostname#nonce` shape: `@` and `#` present, pid is ours,
    /// hostname never empty (HOSTNAME → COMPUTERNAME → "unknown"), nonce
    /// never empty.
    #[test]
    fn process_identity_is_stable_and_wellformed() {
        let first = process_identity();
        let second = process_identity();
        assert_eq!(
            first, second,
            "OnceLock: identity is computed once per process"
        );
        assert!(
            first.contains('@'),
            "expected pid@hostname#nonce, got: {first}"
        );
        assert!(
            first.contains('#'),
            "expected pid@hostname#nonce, got: {first}"
        );
        let (pid_part, rest) = first.split_once('@').expect("has @ separator");
        assert_eq!(
            pid_part,
            std::process::id().to_string(),
            "pid must be the current process pid: {first}"
        );
        let (hostname, nonce) = rest.split_once('#').expect("has # separator");
        assert!(
            !hostname.is_empty(),
            "hostname fallback must never be empty: {first}"
        );
        assert!(!nonce.is_empty(), "nonce must never be empty: {first}");
    }

    /// The metadata-backfill snapshot (btw/subagent rows first written
    /// without a parent link, and pre-table rows written by
    /// `backfill_sessions` without a model) must carry a fresh
    /// `last_active_at` so the list view (max `last_active_at` per
    /// session) picks it up, preserve the immutable columns, prefer the
    /// caller's `model`/`role` when supplied, and leave `writer` unstamped
    /// (insert_meta stamps it at write time). `None` parent/model leave
    /// the existing links/values untouched (model-only backfill case).
    #[test]
    fn backfill_meta_snapshot_preserves_immutable_columns() {
        let created = us_to_datetime(next_event_time_us());
        let existing = SessionMeta {
            session_id: "btw-abc123".into(),
            created_at: created,
            last_active_at: created,
            model: Some("model-x".into()),
            role: Some("main".into()),
            entry_count: 3,
            parent_session_id: None,
            parent_task_id: None,
            title: Some("a title".into()),
            pinned: Some(true),
            archived: Some(true),
            writer: None,
            label: None,
        };
        let now = us_to_datetime(next_event_time_us());
        let backfilled = GreptimeSession::backfill_meta_snapshot(
            &existing,
            now,
            Some("model-y"),
            None,
            Some("parent-1"),
            Some(7),
        );
        assert_eq!(backfilled.session_id, "btw-abc123");
        assert_eq!(
            backfilled.created_at, created,
            "backfill must preserve the original creation time"
        );
        assert_eq!(
            backfilled.last_active_at, now,
            "backfill must carry a fresh last_active_at so the list picks it up"
        );
        assert_eq!(
            backfilled.model.as_deref(),
            Some("model-y"),
            "caller-supplied model wins over the existing value"
        );
        assert_eq!(
            backfilled.role.as_deref(),
            Some("main"),
            "existing role kept when the caller passes none"
        );
        assert_eq!(backfilled.entry_count, 3, "entry_count preserved");
        assert_eq!(backfilled.parent_session_id.as_deref(), Some("parent-1"));
        assert_eq!(backfilled.parent_task_id, Some(7));
        assert_eq!(backfilled.title.as_deref(), Some("a title"));
        assert_eq!(backfilled.pinned, Some(true));
        assert_eq!(backfilled.archived, Some(true));
        assert_eq!(
            backfilled.writer, None,
            "writer is stamped by insert_meta, never by the constructor"
        );

        // Model-only backfill: a caller with a model but no parent fills
        // the missing model and leaves the (absent) links untouched.
        let no_parent = SessionMeta {
            session_id: "btw-abc123".into(),
            created_at: created,
            last_active_at: created,
            model: None,
            role: None,
            entry_count: 3,
            parent_session_id: None,
            parent_task_id: None,
            title: None,
            pinned: None,
            archived: None,
            writer: None,
            label: None,
        };
        let now2 = us_to_datetime(next_event_time_us());
        let model_only = GreptimeSession::backfill_meta_snapshot(
            &no_parent,
            now2,
            Some("model-z"),
            None,
            None,
            None,
        );
        assert_eq!(model_only.model.as_deref(), Some("model-z"));
        assert_eq!(model_only.parent_session_id, None, "no parent injected");
        assert_eq!(model_only.parent_task_id, None);
    }

    // ------------------------------------------------------------------
    // advance_next_seq_from_snapshot_len — fallible monotonic advance
    // ------------------------------------------------------------------
    // Pure-unit testing is deferred because GreptimeSession owns a
    // tokio_postgres::Client that cannot be cheaply constructed without
    // a real or mock connection.  The two scenarios below are exercised
    // via integration tests (require GREPTIME_PG).

    #[tokio::test]
    async fn advance_next_seq_rewind_rejected_integration() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-rewind-{}", crate::session::new_id());
        let mut session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        // After connect, next_seq is 0 (empty session → max_seq=-1 → +1 = 0).

        // Advance to 5, then try to rewind to 3.
        session.advance_next_seq_from_snapshot_len(5).unwrap();
        let err = session.advance_next_seq_from_snapshot_len(3).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("rewind"), "got: {msg}");
    }

    #[tokio::test]
    async fn advance_next_seq_toctou_refresh_integration() {
        // This is the same test as toctou_sync_next_seq_from_load but
        // focused specifically on advance_next_seq_from_snapshot_len
        // normal-advance behavior.
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-seqadv-{}", crate::session::new_id());
        let entries = test_entries();
        let mut s = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        s.append(&entries[..3]).await.unwrap();
        assert_eq!(s.next_seq.load(Ordering::Acquire), 3);

        // Normal advance: 3 → 10
        s.advance_next_seq_from_snapshot_len(10).unwrap();
        assert_eq!(s.next_seq.load(Ordering::Acquire), 10);

        // Same value is allowed (no-op advance).
        s.advance_next_seq_from_snapshot_len(10).unwrap();
        assert_eq!(s.next_seq.load(Ordering::Acquire), 10);
    }

    // ------------------------------------------------------------------
    // running_tasks — background-task state table (integration)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn running_tasks_lifecycle() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-rt-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        // Nothing recorded: nothing to report.
        assert!(
            session
                .take_unfinished_tasks(&sid)
                .await
                .unwrap()
                .is_empty()
        );

        session
            .record_task_start(&sid, 1, "sleep 100", None, None)
            .await
            .unwrap();
        session
            .record_task_start(&sid, 2, "cargo build", None, Some("sub-probe"))
            .await
            .unwrap();

        // Records are scoped per session: another session sees none.
        let other = format!("test-gt-rt-other-{}", crate::session::new_id());
        assert!(
            session
                .take_unfinished_tasks(&other)
                .await
                .unwrap()
                .is_empty()
        );

        // Task 1 completes while we are alive: only task 2 stays on record.
        session.clear_task(&sid, 1).await.unwrap();
        assert_eq!(
            session.take_unfinished_tasks(&sid).await.unwrap(),
            vec!["task 2: cargo build (session: sub-probe)".to_string(),]
        );

        // take consumes the rows: a second launch has nothing to report.
        assert!(
            session
                .take_unfinished_tasks(&sid)
                .await
                .unwrap()
                .is_empty()
        );

        // Task ids restart per process: re-inserting id 1 after the delete
        // tombstone must remain visible (fresh started_at wins).
        session
            .record_task_start(&sid, 1, "restarted", None, None)
            .await
            .unwrap();
        assert_eq!(
            session.take_unfinished_tasks(&sid).await.unwrap(),
            vec!["task 1: restarted".to_string()]
        );
    }

    #[tokio::test]
    async fn running_tasks_subagent_lookup_crosses_parent_sessions() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let subagent = format!("sub-rt-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &subagent)
            .await
            .unwrap();

        // Two different parent sessions each left a delegate row for the
        // same subagent session id.
        let parent_a = format!("parent-a-{}", crate::session::new_id());
        let parent_b = format!("parent-b-{}", crate::session::new_id());
        session
            .record_task_start(&parent_a, 10, "probe a", None, Some(&subagent))
            .await
            .unwrap();
        session
            .record_task_start(&parent_b, 11, "probe b", None, Some(&subagent))
            .await
            .unwrap();

        // The subagent's own session id sees no direct rows...
        assert!(
            session
                .take_unfinished_tasks(&subagent)
                .await
                .unwrap()
                .is_empty()
        );
        // ...but the subagent-scoped lookup finds both leftover delegates.
        let labels = session
            .take_unfinished_tasks_for_subagent(&subagent)
            .await
            .unwrap();
        assert_eq!(labels.len(), 2);
        assert!(labels.contains(&"task 10: probe a".to_string()));
        assert!(labels.contains(&"task 11: probe b".to_string())); // Consumed: a second lookup finds nothing.
        assert!(
            session
                .take_unfinished_tasks_for_subagent(&subagent)
                .await
                .unwrap()
                .is_empty()
        );
        // The parents' own take sees nothing either (rows are gone).
        assert!(
            session
                .take_unfinished_tasks(&parent_a)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            session
                .take_unfinished_tasks(&parent_b)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// The `owner` column (process identity of the recording process) and
    /// `unfinished_owner_all_dead` (the server-attach probe): true only
    /// when EVERY surviving row was left by a definitely-dead process;
    /// NULL owner (old row) / live owner → false. Mirrors the SQLite
    /// backend test; requires a live GREPTIME_PG, otherwise skipped.
    #[tokio::test]
    async fn running_tasks_owner_column_liveness_probe() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-owner-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        // No rows → all dead (vacuously: Consume would take nothing).
        assert!(
            session
                .unfinished_owner_all_dead(&sid)
                .await
                .expect("probe on empty table")
        );

        // A row recorded by THIS process (still alive) → not all dead,
        // and the owner column holds our process identity.
        session
            .record_task_start(&sid, 1, "sleep 100", None, None)
            .await
            .unwrap();
        assert!(
            !session
                .unfinished_owner_all_dead(&sid)
                .await
                .expect("probe with live owner"),
            "a live owner must keep the probe conservative (false)"
        );
        let owners: Vec<Option<String>> = session
            .client
            .query(
                "SELECT owner_identity FROM running_tasks \
                 WHERE workspace_id = $1 AND session_id = $2",
                &[&wid, &sid],
            )
            .await
            .expect("read owner column")
            .iter()
            .map(|row| row.get("owner_identity"))
            .collect();
        assert_eq!(
            owners,
            vec![Some(crate::session_store::process_identity().to_owned())],
            "record must carry the recording process identity"
        );

        // Rewrite the owner to a definitely-dead pid → all rows dead →
        // true. A known-host dead pid is probed; a valid legacy `unknown`
        // identity is explicitly treated as dead.
        match probeable_hostname() {
            Some(hostname) => {
                session
                    .client
                    .execute(
                        "UPDATE running_tasks SET owner_identity = $1 \
                         WHERE workspace_id = $2 AND session_id = $3",
                        &[&format!("2000000000@{hostname}#deadbeef"), &wid, &sid],
                    )
                    .await
                    .expect("rewrite owner to dead pid");
                assert!(
                    session
                        .unfinished_owner_all_dead(&sid)
                        .await
                        .expect("probe with dead owner")
                );
            }
            None => {
                session
                    .client
                    .execute(
                        "UPDATE running_tasks SET owner_identity = $1 \
                         WHERE workspace_id = $2 AND session_id = $3",
                        &[&"2000000000@unknown#deadbeef", &wid, &sid],
                    )
                    .await
                    .expect("rewrite owner to legacy unknown pid");
                assert!(
                    session
                        .unfinished_owner_all_dead(&sid)
                        .await
                        .expect("probe with legacy unknown owner")
                );
            }
        }

        // NULL owner (a row written before the column shipped) → alive.
        session
            .client
            .execute(
                "UPDATE running_tasks SET owner_identity = NULL \
                 WHERE workspace_id = $1 AND session_id = $2",
                &[&wid, &sid],
            )
            .await
            .expect("null out owner");
        assert!(
            !session
                .unfinished_owner_all_dead(&sid)
                .await
                .expect("probe with NULL owner"),
            "a NULL owner (old row) must be treated as alive"
        );

        // Consuming the rows still works unchanged (take → empty table →
        // all dead again).
        assert_eq!(session.take_unfinished_tasks(&sid).await.unwrap().len(), 1);
        assert!(
            session
                .unfinished_owner_all_dead(&sid)
                .await
                .expect("probe after consume")
        );
    }

    /// The current hostname exactly as `process_identity` computes it, for
    /// hand-built owner identities in tests.
    fn hostname_now() -> String {
        std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| "unknown".to_owned())
    }

    /// The currently resolved hostname, when it is known, for hand-built
    /// owner identities that exercise the process probe path.
    fn probeable_hostname() -> Option<String> {
        let host = hostname_now();
        (host != "unknown").then_some(host)
    }

    #[tokio::test]
    async fn running_tasks_label_for_subagent_returns_latest() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let subagent = format!("sub-label-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &subagent)
            .await
            .unwrap();
        assert_eq!(
            session.label_for_subagent(&subagent).await.unwrap(),
            None,
            "no rows yet → no label"
        );

        // Two delegate tasks for the same subagent; the newest started_at wins.
        let parent = format!("parent-label-{}", crate::session::new_id());
        session
            .record_task_start(&parent, 20, "first label", None, Some(&subagent))
            .await
            .unwrap();
        session
            .record_task_start(&parent, 21, "latest label", None, Some(&subagent))
            .await
            .unwrap();
        assert_eq!(
            session
                .label_for_subagent(&subagent)
                .await
                .unwrap()
                .as_deref(),
            Some("latest label"),
            "newest running_tasks row wins"
        );

        // Another subagent's rows never leak into this lookup.
        let other = format!("other-label-{}", crate::session::new_id());
        assert_eq!(session.label_for_subagent(&other).await.unwrap(), None);

        // Consuming the rows (task completion/resume) removes the label.
        session
            .take_unfinished_tasks_for_subagent(&subagent)
            .await
            .unwrap();
        assert_eq!(session.label_for_subagent(&subagent).await.unwrap(), None);
    }

    #[tokio::test]
    async fn running_tasks_all_subagent_labels_returns_latest_per_subagent() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let subagent = format!("sub-all-labels-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &subagent)
            .await
            .unwrap();
        assert!(
            session.all_subagent_labels().await.unwrap().is_empty(),
            "no rows yet → empty map"
        );

        // Two delegate tasks for the same subagent; the newest started_at
        // wins (record_task_start timestamps are strictly increasing).
        let parent = format!("parent-all-labels-{}", crate::session::new_id());
        session
            .record_task_start(&parent, 20, "first label", None, Some(&subagent))
            .await
            .unwrap();
        session
            .record_task_start(&parent, 21, "latest label", None, Some(&subagent))
            .await
            .unwrap();
        let labels = session.all_subagent_labels().await.unwrap();
        assert_eq!(
            labels.get(&subagent).and_then(|label| label.as_deref()),
            Some("latest label"),
            "newest running_tasks row wins"
        );

        // Another subagent's rows are in the map too, and rows without a
        // subagent id never leak into it.
        let other = format!("other-all-labels-{}", crate::session::new_id());
        session
            .record_task_start(&parent, 30, "other label", None, Some(&other))
            .await
            .unwrap();
        session
            .record_task_start(&parent, 40, "no subagent", None, None)
            .await
            .unwrap();
        let labels = session.all_subagent_labels().await.unwrap();
        assert_eq!(
            labels.get(&other).and_then(|label| label.as_deref()),
            Some("other label")
        );
        assert_eq!(labels.len(), 2, "rows without a subagent id are excluded");
    }

    // ------------------------------------------------------------------
    // sessions — metadata audit table (integration)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn sessions_meta_create_list_touch_audit_delete() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-meta-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        // create → the list sees it immediately (synchronous create).
        session
            .create_meta(
                &sid,
                Some("model-x"),
                Some("main"),
                None,
                None,
                Some("delegate label"),
            )
            .await
            .unwrap();
        let list = session.list_meta().await.unwrap();
        let created = list
            .iter()
            .find(|m| m.session_id == sid)
            .expect("created session must be listed");
        assert_eq!(created.model.as_deref(), Some("model-x"));
        assert_eq!(created.role.as_deref(), Some("main"));
        assert_eq!(created.entry_count, 0);
        assert_eq!(
            created.title.as_deref(),
            Some("delegate label"),
            "a title supplied at creation is recorded"
        );

        // create is idempotent: a second create appends nothing, and a
        // different title on resume never overwrites the creation title.
        session
            .create_meta(
                &sid,
                Some("model-x"),
                Some("main"),
                None,
                None,
                Some("resume title"),
            )
            .await
            .unwrap();
        assert_eq!(
            session.audit_meta(&sid).await.unwrap().len(),
            1,
            "re-create must not append a second creation snapshot"
        );
        let list = session.list_meta().await.unwrap();
        assert_eq!(
            list.iter()
                .find(|m| m.session_id == sid)
                .unwrap()
                .title
                .as_deref(),
            Some("delegate label"),
            "resume keeps the creation title"
        );

        // touch twice → audit trail has 3 rows (create + 2 touches), the
        // list still returns one latest snapshot, created_at survives.
        session.touch_meta().await.unwrap();
        session.touch_meta().await.unwrap();
        let trail = session.audit_meta(&sid).await.unwrap();
        assert_eq!(trail.len(), 3, "create + 2 touches = 3 audit rows");
        let list = session.list_meta().await.unwrap();
        let latest = list
            .iter()
            .find(|m| m.session_id == sid)
            .expect("session still listed after touches");
        assert_eq!(
            latest.created_at, created.created_at,
            "touch must preserve created_at"
        );
        assert!(
            latest.last_active_at > created.last_active_at,
            "touch must advance last_active_at"
        );

        // delete hides the session entirely (all audit rows gone).
        session.delete_meta(&sid).await.unwrap();
        assert!(
            session.audit_meta(&sid).await.unwrap().is_empty(),
            "delete removes the full audit trail"
        );
        assert!(
            !session
                .list_meta()
                .await
                .unwrap()
                .iter()
                .any(|m| m.session_id == sid),
            "deleted session must not be listed"
        );
    }

    /// Every snapshot row is stamped with the writing process's identity
    /// (`pid@hostname#nonce`): after create_meta + touch, both the list
    /// (latest snapshot) and the audit trail (every row) carry
    /// `writer == process_identity()`.
    #[tokio::test]
    async fn sessions_meta_writer_is_process_identity() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-meta-writer-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        // create → the snapshot row is stamped with this process's identity.
        session
            .create_meta(&sid, Some("model-x"), None, None, None, None)
            .await
            .unwrap();
        session.touch_meta().await.unwrap();
        session.touch_meta().await.unwrap();

        // The list view (latest snapshot per session) carries the writer.
        let list = session.list_meta().await.unwrap();
        let latest = list
            .iter()
            .find(|m| m.session_id == sid)
            .expect("created session must be listed");
        assert_eq!(
            latest.writer.as_deref(),
            Some(process_identity()),
            "latest snapshot writer must be this process's identity"
        );

        // The audit trail stamps every row, not just the newest.
        for row in session.audit_meta(&sid).await.unwrap() {
            assert_eq!(
                row.writer.as_deref(),
                Some(process_identity()),
                "every audit row is stamped with the writing process"
            );
        }

        // A fresh connect re-reads the stamped writer from the DB (the
        // cached row is built from the table, not reconstructed).
        let resumed = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        let resumed_latest = resumed
            .list_meta()
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.session_id == sid)
            .expect("resumed session must still be listed");
        assert_eq!(
            resumed_latest.writer.as_deref(),
            Some(process_identity()),
            "reconnect must read the writer back from the snapshot row"
        );
    }

    #[tokio::test]
    async fn sessions_meta_entry_count_is_next_seq() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-meta-seq-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        let entries = test_entries();

        // entry_count = next_seq (MAX(seq)+1 at connect, advanced by
        // append) — not a physical row count.
        session.append(&entries[..3]).await.unwrap();
        assert_eq!(session.next_seq.load(Ordering::Acquire), 3);
        session
            .create_meta(&sid, Some("m"), None, None, None, None)
            .await
            .unwrap();
        let list = session.list_meta().await.unwrap();
        let meta = list
            .iter()
            .find(|m| m.session_id == sid)
            .expect("created session must be listed");
        assert_eq!(meta.entry_count, 3, "create carries next_seq");

        // A touch after more appends carries the advanced next_seq.
        session.append(&entries[3..5]).await.unwrap();
        assert_eq!(session.next_seq.load(Ordering::Acquire), 5);
        session.touch_meta().await.unwrap();
        let list = session.list_meta().await.unwrap();
        let latest = list
            .iter()
            .find(|m| m.session_id == sid)
            .expect("session still listed after touch");
        assert_eq!(latest.entry_count, 5, "touch carries next_seq");
    }

    #[tokio::test]
    async fn sessions_meta_touch_never_self_creates() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-meta-nocreate-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        // No row exists (fresh subagent whose parent has not written yet):
        // a touch must skip, never fabricate its own row (R3).
        session.touch_meta().await.unwrap();
        assert!(
            session.audit_meta(&sid).await.unwrap().is_empty(),
            "touch must not self-create a row"
        );

        // Once the parent's row lands, the next touch reads it back
        // (read-on-miss) and appends a complete snapshot carrying the
        // immutable columns.
        session
            .create_meta(
                &sid,
                Some("sub-model"),
                Some("fixer"),
                Some("parent-x"),
                Some(7),
                None,
            )
            .await
            .unwrap();
        session.touch_meta().await.unwrap();
        let trail = session.audit_meta(&sid).await.unwrap();
        assert_eq!(trail.len(), 2, "parent create + subagent touch");
        let latest = session
            .list_meta()
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.session_id == sid)
            .expect("session listed");
        assert_eq!(latest.model.as_deref(), Some("sub-model"));
        assert_eq!(latest.role.as_deref(), Some("fixer"));
        assert_eq!(latest.parent_session_id.as_deref(), Some("parent-x"));
        assert_eq!(latest.parent_task_id, Some(7));
    }

    /// btw/subagent parent-link backfill: `SessionFactory::build` writes
    /// the first row with parent = None, and the parent process's later
    /// `create_meta` records the real link by appending ONE fresh snapshot
    /// (append-only, never UPDATE). The backfill is idempotent — re-
    /// recording the same session appends nothing once the link is set —
    /// and an existing parent link is never overwritten.
    #[tokio::test]
    async fn sessions_meta_backfills_parent_link_once() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-meta-backfill-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        // 1. build writes the first row with parent = None.
        session
            .create_meta(&sid, Some("model-x"), Some("main"), None, None, None)
            .await
            .unwrap();
        let trail = session.audit_meta(&sid).await.unwrap();
        assert_eq!(trail.len(), 1);
        assert_eq!(trail[0].parent_session_id, None);
        let original_created_at = trail[0].created_at;

        // 2. the parent records the real link → one fresh row is appended,
        //    the old row is kept, and the latest snapshot carries the link.
        session
            .create_meta(
                &sid,
                Some("model-x"),
                Some("main"),
                Some("parent-1"),
                Some(7),
                None,
            )
            .await
            .unwrap();
        let trail = session.audit_meta(&sid).await.unwrap();
        assert_eq!(trail.len(), 2, "backfill appends one row, old row kept");
        let latest = session
            .list_meta()
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.session_id == sid)
            .expect("session listed");
        assert_eq!(latest.parent_session_id.as_deref(), Some("parent-1"));
        assert_eq!(latest.parent_task_id, Some(7));
        assert_eq!(
            latest.created_at, original_created_at,
            "backfill must preserve the original creation time"
        );

        // 3. idempotent: re-recording the same parent appends nothing.
        session
            .create_meta(
                &sid,
                Some("model-x"),
                Some("main"),
                Some("parent-1"),
                Some(7),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            session.audit_meta(&sid).await.unwrap().len(),
            2,
            "re-backfill with the same parent must not append"
        );

        // 4. a different parent never overwrites the recorded link.
        session
            .create_meta(
                &sid,
                Some("model-x"),
                Some("main"),
                Some("parent-2"),
                Some(8),
                None,
            )
            .await
            .unwrap();
        let trail = session.audit_meta(&sid).await.unwrap();
        assert_eq!(
            trail.len(),
            2,
            "existing parent link must not be overwritten"
        );
        let latest = session
            .list_meta()
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.session_id == sid)
            .expect("session listed");
        assert_eq!(
            latest.parent_session_id.as_deref(),
            Some("parent-1"),
            "the first recorded link wins"
        );

        // 5. create_meta without a parent over an existing parent-less row
        //    appends nothing (the plain resume path stays a no-op).
        let sid2 = format!(
            "test-gt-meta-backfill-noparent-{}",
            crate::session::new_id()
        );
        let session2 = GreptimeSession::connect(&conn, &wid, &sid2).await.unwrap();
        session2
            .create_meta(&sid2, Some("model-x"), Some("main"), None, None, None)
            .await
            .unwrap();
        session2
            .create_meta(&sid2, Some("model-x"), Some("main"), None, None, None)
            .await
            .unwrap();
        assert_eq!(
            session2.audit_meta(&sid2).await.unwrap().len(),
            1,
            "parent-less create over an existing parent-less row must not append"
        );

        session.delete_meta(&sid).await.unwrap();
        session2.delete_meta(&sid2).await.unwrap();
    }

    /// Missing-model backfill: rows written by `backfill_sessions`
    /// (pre-table era) carry `model = NULL`; the next `create_meta` that
    /// knows the model (the parent process at btw/subagent spawn, or
    /// `SessionFactory::build` on a main-session resume) appends ONE fresh
    /// snapshot filling the model. Idempotent — once the model is set, a
    /// later create_meta appends nothing; an existing model is never
    /// overwritten.
    #[tokio::test]
    async fn sessions_meta_backfills_missing_model_once() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-meta-model-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        // 1. a pre-table row: model NULL, no parent (backfill_sessions
        //    signature — create_meta with model/parent None).
        session
            .create_meta(&sid, None, None, None, None, None)
            .await
            .unwrap();
        let trail = session.audit_meta(&sid).await.unwrap();
        assert_eq!(trail.len(), 1);
        assert_eq!(trail[0].model, None);
        let original_created_at = trail[0].created_at;

        // 2. btw spawn's create_meta (model + parent) → one fresh row fills
        //    both; old row kept; latest snapshot carries model + parent.
        session
            .create_meta(&sid, Some("model-y"), None, Some("parent-1"), Some(7), None)
            .await
            .unwrap();
        let trail = session.audit_meta(&sid).await.unwrap();
        assert_eq!(trail.len(), 2, "backfill appends one row, old row kept");
        let latest = session
            .list_meta()
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.session_id == sid)
            .expect("session listed");
        assert_eq!(latest.model.as_deref(), Some("model-y"));
        assert_eq!(latest.parent_session_id.as_deref(), Some("parent-1"));
        assert_eq!(latest.parent_task_id, Some(7));
        assert_eq!(
            latest.created_at, original_created_at,
            "backfill must preserve the original creation time"
        );

        // 3. idempotent: re-recording with the same model appends nothing.
        session
            .create_meta(&sid, Some("model-y"), None, Some("parent-1"), Some(7), None)
            .await
            .unwrap();
        assert_eq!(
            session.audit_meta(&sid).await.unwrap().len(),
            2,
            "re-backfill with the model already set must not append"
        );

        // 4. a different model never overwrites the recorded one.
        session
            .create_meta(&sid, Some("model-z"), None, Some("parent-1"), Some(7), None)
            .await
            .unwrap();
        let latest = session
            .list_meta()
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.session_id == sid)
            .expect("session listed");
        assert_eq!(
            latest.model.as_deref(),
            Some("model-y"),
            "the first recorded model wins"
        );

        session.delete_meta(&sid).await.unwrap();
    }

    #[tokio::test]
    async fn sessions_meta_set_title_persists_and_survives_touch() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-title-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        // set_title on a session with no row yet is a no-op Ok (R3: never
        // self-create), mirroring touch_meta.
        session.set_title(&sid, Some("ghost title")).await.unwrap();
        assert!(
            session.audit_meta(&sid).await.unwrap().is_empty(),
            "set_title must not self-create a row"
        );

        session
            .create_meta(&sid, Some("model-x"), Some("main"), None, None, None)
            .await
            .unwrap();

        // Rename → the list shows the new title, the audit trail appends
        // one snapshot, created_at survives.
        session.set_title(&sid, Some("My Session")).await.unwrap();
        let list = session.list_meta().await.unwrap();
        let renamed = list
            .iter()
            .find(|m| m.session_id == sid)
            .expect("session still listed after rename");
        assert_eq!(renamed.title.as_deref(), Some("My Session"));
        assert_eq!(
            renamed.model.as_deref(),
            Some("model-x"),
            "rename preserves other columns"
        );
        assert_eq!(
            session.audit_meta(&sid).await.unwrap().len(),
            2,
            "create + rename = 2 audit rows"
        );

        // touch carries the title: the append-only snapshot semantics mean
        // the newest row still has it.
        session.touch_meta().await.unwrap();
        let latest = session
            .list_meta()
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.session_id == sid)
            .expect("session listed after touch");
        assert_eq!(
            latest.title.as_deref(),
            Some("My Session"),
            "touch preserves title"
        );
        assert_eq!(
            latest.created_at, renamed.created_at,
            "touch must preserve created_at"
        );

        // Clearing (None) stores NULL and is visible on the read path.
        session.set_title(&sid, None).await.unwrap();
        let latest = session
            .list_meta()
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.session_id == sid)
            .expect("session listed after clear");
        assert_eq!(latest.title, None, "clear stores NULL");
    }

    #[tokio::test]
    async fn sessions_meta_set_pinned_persists_and_survives_touch() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-pin-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        // set_pinned on a session with no row yet is a no-op Ok (R3: never
        // self-create), mirroring set_title / touch_meta.
        session.set_pinned(&sid, true).await.unwrap();
        assert!(
            session.audit_meta(&sid).await.unwrap().is_empty(),
            "set_pinned must not self-create a row"
        );

        session
            .create_meta(&sid, Some("model-x"), Some("main"), None, None, None)
            .await
            .unwrap();
        let created = session
            .list_meta()
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.session_id == sid)
            .expect("created session must be listed");
        assert_eq!(
            created.pinned, None,
            "a fresh session reads as never-touched (None)"
        );

        // Pin → the list shows pinned=true, the audit trail appends one
        // snapshot, created_at survives.
        session.set_pinned(&sid, true).await.unwrap();
        let list = session.list_meta().await.unwrap();
        let pinned = list
            .iter()
            .find(|m| m.session_id == sid)
            .expect("session still listed after pin");
        assert_eq!(pinned.pinned, Some(true));
        assert_eq!(
            pinned.model.as_deref(),
            Some("model-x"),
            "pin preserves other columns"
        );
        assert_eq!(
            session.audit_meta(&sid).await.unwrap().len(),
            2,
            "create + pin = 2 audit rows"
        );

        // touch carries the pin: the append-only snapshot semantics mean
        // the newest row still has it.
        session.touch_meta().await.unwrap();
        let latest = session
            .list_meta()
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.session_id == sid)
            .expect("session listed after touch");
        assert_eq!(latest.pinned, Some(true), "touch preserves the pin flag");
        assert_eq!(
            latest.created_at, pinned.created_at,
            "touch must preserve created_at"
        );

        // Unpin stores Some(false) — distinct from the None of a
        // never-touched session, both read as unpinned.
        session.set_pinned(&sid, false).await.unwrap();
        let latest = session
            .list_meta()
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.session_id == sid)
            .expect("session listed after unpin");
        assert_eq!(latest.pinned, Some(false), "unpin stores Some(false)");
    }

    #[tokio::test]
    async fn sessions_meta_set_archived_persists_and_survives_touch() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-archive-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        // set_archived on a session with no row yet is a no-op Ok (R3:
        // never self-create), mirroring set_pinned / set_title / touch_meta.
        session.set_archived(&sid, true).await.unwrap();
        assert!(
            session.audit_meta(&sid).await.unwrap().is_empty(),
            "set_archived must not self-create a row"
        );

        session
            .create_meta(&sid, Some("model-x"), Some("main"), None, None, None)
            .await
            .unwrap();
        let created = session
            .list_meta()
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.session_id == sid)
            .expect("created session must be listed");
        assert_eq!(
            created.archived, None,
            "a fresh session reads as never-touched (None)"
        );

        // Archive → the list shows archived=true, the audit trail appends
        // one snapshot, created_at survives.
        session.set_archived(&sid, true).await.unwrap();
        let list = session.list_meta().await.unwrap();
        let archived = list
            .iter()
            .find(|m| m.session_id == sid)
            .expect("session still listed after archive");
        assert_eq!(archived.archived, Some(true));
        assert_eq!(
            archived.model.as_deref(),
            Some("model-x"),
            "archive preserves other columns"
        );
        assert_eq!(
            session.audit_meta(&sid).await.unwrap().len(),
            2,
            "create + archive = 2 audit rows"
        );

        // touch carries the archived flag: the append-only snapshot
        // semantics mean the newest row still has it.
        session.touch_meta().await.unwrap();
        let latest = session
            .list_meta()
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.session_id == sid)
            .expect("session listed after touch");
        assert_eq!(
            latest.archived,
            Some(true),
            "touch preserves the archived flag"
        );
        assert_eq!(
            latest.created_at, archived.created_at,
            "touch must preserve created_at"
        );

        // Restore stores Some(false) — distinct from the None of a
        // never-touched session, both read as unarchived.
        session.set_archived(&sid, false).await.unwrap();
        let latest = session
            .list_meta()
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.session_id == sid)
            .expect("session listed after restore");
        assert_eq!(latest.archived, Some(false), "restore stores Some(false)");
    }

    #[tokio::test]
    async fn sessions_meta_backfill_is_idempotent() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        // Isolated workspace: backfill aggregates per workspace and the
        // other tests share the default workspace id.
        let wid = derive_workspace_id(Path::new("/tmp/e-agent-backfill-test"));
        let sid = format!("test-gt-bf-{}", crate::session::new_id());
        let session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        let entries = test_entries();

        // Pre-table session: transcript rows, no metadata rows.
        session.append(&entries[..3]).await.unwrap();
        assert!(session.audit_meta(&sid).await.unwrap().is_empty());

        session.backfill_sessions().await.unwrap();
        let list = session.list_meta().await.unwrap();
        // Scoped to this session, not the whole workspace: `backfill_sessions`
        // aggregates per workspace, and earlier runs of this very test leave
        // `session_entries` behind (there is no entries-deletion API), so a
        // repeat run on the same DB re-creates their meta rows and the
        // workspace is never empty.
        let mine: Vec<_> = list.iter().filter(|m| m.session_id == sid).collect();
        assert_eq!(
            mine.len(),
            1,
            "backfill created exactly one row for this session"
        );
        let meta = mine[0];
        assert_eq!(meta.entry_count, 3, "MAX(seq)+1, not COUNT");
        assert_eq!(meta.model, None, "pre-table sessions have no model");

        // Idempotent: a second run inserts nothing; this session's row is
        // identical.
        session.backfill_sessions().await.unwrap();
        let list = session.list_meta().await.unwrap();
        let mine: Vec<_> = list.iter().filter(|m| m.session_id == sid).collect();
        assert_eq!(mine.len(), 1, "second backfill adds no second row");
        assert_eq!(mine[0].entry_count, 3);
        assert_eq!(mine[0].created_at, meta.created_at);
        assert_eq!(mine[0].last_active_at, meta.last_active_at);
        assert_eq!(
            session.audit_meta(&sid).await.unwrap().len(),
            1,
            "second backfill appends nothing"
        );

        // Clean up this run's meta row (entries stay — no deletion API).
        session.delete_meta(&sid).await.unwrap();
    }
}
