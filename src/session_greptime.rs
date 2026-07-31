//! GreptimeDB-backed session storage. Optional runtime-selectable
//! backend via `[session] backend = "greptime"`. Still experimental.
//! Uses tokio-postgres for both read and write against the
//! `session_entries` transcript table and the `running_tasks` state table.
//!
//! Non-goals: no Storage trait, no migration of existing JSONL sessions.

use std::path::Path;

use anyhow::{Context, Result};
use tokio_postgres::NoTls;

use crate::agent::{Message, SessionEntry};
use crate::session_store::SessionMeta;

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
    subagent_session_id STRING NULL,
    started_at TIMESTAMP(9) NOT NULL TIME INDEX,
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
/// (created_at/model/role/parent/entry_count all carried on every row) at a
/// fresh `last_active_at`; the TIME INDEX keeps the rows ordered, and the
/// list view deduplicates per primary key taking the latest `last_active_at`
/// (explicitly in SQL — the query is correct whether or not the engine
/// auto-dedups same-PK rows). Because each row is a full snapshot, a touch
/// can never wipe immutable columns: there is no partial-row rewrite at all.
const CREATE_TABLE_SESSIONS: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    workspace_id STRING NOT NULL,
    session_id STRING NOT NULL,
    created_at TIMESTAMP(9) NOT NULL,
    last_active_at TIMESTAMP(9) NOT NULL,
    model STRING NULL,
    "role" STRING NULL,
    entry_count BIGINT NOT NULL DEFAULT 0,
    parent_session_id STRING NULL,
    parent_task_id BIGINT NULL,
    TIME INDEX (last_active_at),
    PRIMARY KEY (workspace_id, session_id)
)
"#;

/// Derive a stable workspace identifier from the canonical workspace root
/// path. This mirrors how the JSONL backend uses the root path as a
/// namespace (different workspaces get different on-disk directories).
pub fn derive_workspace_id(root: &Path) -> String {
    root.to_string_lossy().to_string()
}

/// Monotonic real-time microsecond timestamp. Uses wall clock but guarantees
/// strict ordering within a process: if two entries land in the same
/// microsecond (or the clock goes backwards), the later one gets prev+1.
/// Returns microseconds since Unix epoch.
///
/// Precision note: `TIMESTAMP(9)` stores up to nanosecond precision, but
/// this function only guarantees microsecond granularity. Timestamps from
/// this function are stored as microseconds-since-epoch and converted to
/// nanoseconds for the `chrono::NaiveDateTime` used by tokio-postgres.
fn next_event_time_us() -> i64 {
    use std::sync::atomic::{AtomicI64, Ordering};
    static LAST: AtomicI64 = AtomicI64::new(0);
    let now_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64;
    loop {
        let prev = LAST.load(Ordering::Relaxed);
        let ts = if now_us > prev { now_us } else { prev + 1 };
        if LAST
            .compare_exchange(prev, ts, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return ts;
        }
    }
}

/// Convert microseconds-since-epoch to chrono NaiveDateTime for tokio-postgres.
fn us_to_datetime(us: i64) -> chrono::NaiveDateTime {
    chrono::DateTime::from_timestamp(us / 1_000_000, ((us % 1_000_000) * 1000) as u32)
        .unwrap()
        .naive_utc()
}

/// Classify a SessionEntry for the entry_kind column.
fn entry_kind(entry: &SessionEntry) -> &'static str {
    match entry {
        SessionEntry::Message { .. } => "message",
        SessionEntry::Compaction { .. } => "compaction",
        SessionEntry::Notice { .. } => "notice",
        SessionEntry::BackgroundCompletion { .. } => "background_completion",
        SessionEntry::ForkedFrom { .. } => "forked_from",
    }
}

/// Whether a message is a tool error.
fn is_error(entry: &SessionEntry) -> bool {
    match entry {
        SessionEntry::Message {
            message: Message::Tool { is_error, .. },
        } => *is_error,
        _ => false,
    }
}

pub struct GreptimeSession {
    client: tokio_postgres::Client,
    /// Next sequence number for appends within this session.
    next_seq: i64,
    workspace_id: String,
    session_id: String,
    /// The session's latest metadata snapshot, cached at connect time so
    /// every touch carries the immutable columns (created_at/model/role/
    /// parent) without re-reading them. `None` = no row yet (brand-new
    /// session, or a subagent whose parent has not written its row yet).
    /// Interior mutability because every touch path takes `&self` while
    /// the store hands the session out behind a tokio Mutex.
    cached_meta: std::sync::Mutex<Option<SessionMeta>>,
}

impl GreptimeSession {
    /// Connect and ensure the table exists. `conn` is a tokio-postgres
    /// connection string, e.g. "host=127.0.0.1 port=4002 dbname=public".
    /// `workspace_id` is derived from the canonical workspace root and
    /// scopes sessions to their workspace.
    pub async fn connect(conn: &str, workspace_id: &str, session_id: &str) -> Result<Self> {
        let (client, connection) = tokio_postgres::connect(conn, NoTls)
            .await
            .context("cannot connect to GreptimeDB")?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("greptime session connection error: {e}");
            }
        });
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

        // Cache the session's latest metadata snapshot (if any) so later
        // touches rewrite complete rows without re-reading immutable
        // columns. The table is append-only; the newest row per session
        // wins (ORDER BY last_active_at DESC LIMIT 1).
        let meta_row = client
            .query_opt(
                "SELECT created_at, last_active_at, model, \"role\", entry_count, \
                        parent_session_id, parent_task_id \
                 FROM sessions \
                 WHERE workspace_id = $1 AND session_id = $2 \
                 ORDER BY last_active_at DESC LIMIT 1",
                &[&workspace_id, &session_id],
            )
            .await
            .context("cannot query session metadata")?;
        let cached_meta = meta_row.map(|row| SessionMeta {
            session_id: session_id.to_string(),
            created_at: row.get("created_at"),
            last_active_at: row.get("last_active_at"),
            model: row.get("model"),
            role: row.get("role"),
            entry_count: row.get("entry_count"),
            parent_session_id: row.get("parent_session_id"),
            parent_task_id: row.get("parent_task_id"),
        });

        // Find the current max seq via COALESCE(MAX(seq), -1).
        // Returns -1 for an empty session (no rows for the workspace/session).
        // Scans the full session partition, which is acceptable because
        // sessions are append-only and bounded by typical turn counts.
        let row = client
            .query_one(
                "SELECT COALESCE(MAX(seq), -1)::BIGINT AS max_seq \
                 FROM session_entries \
                 WHERE workspace_id = $1 AND session_id = $2",
                &[&workspace_id, &session_id],
            )
            .await
            .context("cannot query max seq")?;
        let max_seq: i64 = row.get("max_seq");

        let next_seq = max_seq.checked_add(1).context(format!(
            "max_seq overflow in connect for session '{session_id}'"
        ))?;

        Ok(Self {
            client,
            next_seq,
            workspace_id: workspace_id.to_string(),
            session_id: session_id.to_string(),
            cached_meta: std::sync::Mutex::new(cached_meta),
        })
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
        if next < self.next_seq {
            anyhow::bail!(
                "next_seq rewind: current={}, requested={}; \
                 monotonic advance only (use snapshot length, not DB row count)",
                self.next_seq,
                next,
            );
        }
        self.next_seq = next;
        Ok(())
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

        dedup_raw_entries(&raw, &self.session_id, &self.workspace_id)
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

        dedup_raw_entries(&raw, &self.session_id, &self.workspace_id)
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
    /// ascending order.
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
                            "SELECT seq, event_time, payload FROM session_entries \
                             WHERE workspace_id = $1 AND session_id = $2 \
                             AND seq >= $3 AND seq < $4 \
                             ORDER BY seq DESC LIMIT $5",
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
                            "SELECT seq, event_time, payload FROM session_entries \
                             WHERE workspace_id = $1 AND session_id = $2 AND seq < $3 \
                             ORDER BY seq DESC LIMIT $4",
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

        let entries = dedup_raw_entries(&raw, &self.session_id, &self.workspace_id)?
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

        let entries = dedup_raw_entries(&raw, &self.session_id, &self.workspace_id)?
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

        let entries = dedup_raw_entries(&raw, &self.session_id, &self.workspace_id)?
            .into_iter()
            .map(|(_, e)| e)
            .collect();
        Ok((entries, next_comp))
    }

    /// Append new entries atomically per multi-row INSERT statement.
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
    /// batch are folded by the read path.
    pub async fn append(&mut self, entries: &[SessionEntry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let n = entries.len();
        const CHUNK_SIZE: usize = 9000;

        // Validate the complete write range before any DB write.
        // All seqs from base_seq..base_seq+n must be representable as i64
        // and the final cursor must also be in range.
        let base_seq = self.next_seq;
        let n_i64 = i64::try_from(n).context("append count overflowed i64")?;
        let final_cursor = base_seq.checked_add(n_i64).ok_or_else(|| {
            anyhow::anyhow!(
                "seq range overflow: base_seq={base_seq} count={n}; \
                 max seq would exceed i64::MAX"
            )
        })?;

        // Serialize all entries upfront so serialization errors happen
        // before any database write.
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

        // Advance next_seq only after all chunks succeed, so a partial
        // failure does not shift sequence numbers on retry.
        self.next_seq = final_cursor;

        Ok(())
    }

    // ------------------------------------------------------------------
    // sessions — metadata audit table
    // ------------------------------------------------------------------
    //
    // Greptime has no UPDATE: "upsert" is a same-PK INSERT and
    // last-write-wins by the TIME INDEX. The sessions table is an
    // append-only lifecycle audit log: every create/touch appends a
    // COMPLETE snapshot row (created_at/model/role/parent/entry_count all
    // carried on every row), and the list view deduplicates per session
    // taking the latest last_active_at. There is no partial-row rewrite,
    // so a touch can never wipe immutable columns.

    /// Latest metadata snapshot for one session, or `None` when the
    /// session has no row yet (brand-new session, or a subagent whose
    /// parent has not written its row yet).
    async fn load_meta_row(&self, session_id: &str) -> Result<Option<SessionMeta>> {
        let row = self
            .client
            .query_opt(
                "SELECT created_at, last_active_at, model, \"role\", entry_count, \
                        parent_session_id, parent_task_id \
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
        }))
    }

    async fn insert_meta(&self, meta: &SessionMeta) -> Result<()> {
        self.client
            .execute(
                "INSERT INTO sessions \
                 (workspace_id, session_id, created_at, last_active_at, model, \"role\", \
                  entry_count, parent_session_id, parent_task_id) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
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
                ],
            )
            .await
            .context("cannot insert session metadata")?;
        Ok(())
    }

    /// The full snapshot the next touch must carry: the cached row when
    /// present, otherwise a read-on-miss of the table (a subagent's first
    /// touch reads back the row its parent wrote at spawn time — R3).
    /// Absence is never cached: a parent may create the row between
    /// touches, and the next touch must see it.
    async fn effective_meta(&self) -> Result<Option<SessionMeta>> {
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

    /// Create the session's first metadata snapshot row. Synchronous
    /// (awaited by the caller) so the list sees the session immediately.
    ///
    /// Idempotent per session: when a row already exists this is a resume,
    /// not a creation, so nothing is appended (a second creation snapshot
    /// would rewrite `created_at` and pollute the audit log). `model` /
    /// `role` / parent links are supplied by the caller — the parent
    /// process writes subagent rows at spawn time; the main session's row
    /// is written by `SessionFactory::build`.
    pub async fn create_meta(
        &self,
        session_id: &str,
        model: Option<&str>,
        role: Option<&str>,
        parent_session_id: Option<&str>,
        parent_task_id: Option<i64>,
    ) -> Result<()> {
        if self.load_meta_row(session_id).await?.is_some() {
            return Ok(());
        }
        let now = us_to_datetime(next_event_time_us());
        let meta = SessionMeta {
            session_id: session_id.to_owned(),
            created_at: now,
            last_active_at: now,
            model: model.map(str::to_owned),
            role: role.map(str::to_owned),
            entry_count: self.next_seq,
            parent_session_id: parent_session_id.map(str::to_owned),
            parent_task_id,
        };
        self.insert_meta(&meta).await?;
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
        meta.entry_count = self.next_seq;
        self.insert_meta(&meta).await
    }

    /// Latest metadata snapshot per session, newest activity first.
    ///
    /// The table is append-only, so one session has many rows; the list
    /// deduplicates by primary key keeping the row with the maximum
    /// `last_active_at` per session (explicit GROUP BY + JOIN — correct
    /// whether or not the engine auto-dedups same-PK rows at read time).
    pub async fn list_meta(&self) -> Result<Vec<SessionMeta>> {
        let rows = self
            .client
            .query(
                "SELECT s.session_id, s.created_at, s.last_active_at, s.model, s.\"role\", \
                        s.entry_count, s.parent_session_id, s.parent_task_id \
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
        Ok(rows
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
            })
            .collect())
    }

    /// The full lifecycle trace of one session: every snapshot row, oldest
    /// activity first. This is the audit view — [`Self::list_meta`] shows
    /// only the latest snapshot per session.
    pub async fn audit_meta(&self, session_id: &str) -> Result<Vec<SessionMeta>> {
        let rows = self
            .client
            .query(
                "SELECT created_at, last_active_at, model, \"role\", entry_count, \
                        parent_session_id, parent_task_id \
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
            self.insert_meta(&SessionMeta {
                session_id,
                created_at: row.get("created_at"),
                last_active_at: row.get("last_active_at"),
                model: None,
                role: None,
                entry_count: row.get("entry_count"),
                parent_session_id: None,
                parent_task_id: None,
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
    pub async fn record_task_start(
        &self,
        session_id: &str,
        task_id: u64,
        label: &str,
        subagent_session_id: Option<&str>,
    ) -> Result<()> {
        let started_at = us_to_datetime(next_event_time_us());
        let task_id =
            i64::try_from(task_id).context("background task id does not fit in BIGINT")?;
        self.client
            .execute(
                "INSERT INTO running_tasks \
                 (workspace_id, session_id, task_id, label, subagent_session_id, started_at) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[
                    &self.workspace_id,
                    &session_id,
                    &task_id,
                    &label,
                    &subagent_session_id,
                    &started_at,
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

    /// Tasks recorded by a previous process that died before their
    /// completion arrived. Consumes (deletes) all rows for the session and
    /// returns the labels so the caller can inject the "killed on exit"
    /// notice. Rows scoped to another session are untouched.
    pub async fn take_unfinished_tasks(&self, session_id: &str) -> Result<Vec<String>> {
        let rows = self
            .client
            .query(
                "SELECT task_id, label, subagent_session_id FROM running_tasks \
                 WHERE workspace_id = $1 AND session_id = $2",
                &[&self.workspace_id, &session_id],
            )
            .await
            .context("cannot load unfinished background tasks")?;
        let labels = rows
            .iter()
            .map(|row| {
                let task_id: i64 = row.get("task_id");
                let label: String = row.get("label");
                let subagent: Option<String> = row.get("subagent_session_id");
                crate::session::format_unfinished(
                    task_id.max(0) as u64,
                    &label,
                    subagent.as_deref(),
                )
            })
            .collect::<Vec<_>>();
        if !rows.is_empty() {
            self.client
                .execute(
                    "DELETE FROM running_tasks \
                     WHERE workspace_id = $1 AND session_id = $2",
                    &[&self.workspace_id, &session_id],
                )
                .await
                .context("cannot clear unfinished background tasks")?;
        }
        Ok(labels)
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
    ) -> Result<Vec<String>> {
        let rows = self
            .client
            .query(
                "SELECT task_id, label FROM running_tasks \
                 WHERE workspace_id = $1 AND subagent_session_id = $2",
                &[&self.workspace_id, &subagent_session_id],
            )
            .await
            .context("cannot load unfinished subagent tasks")?;
        let labels = rows
            .iter()
            .map(|row| {
                let task_id: i64 = row.get("task_id");
                let label: String = row.get("label");
                crate::session::format_unfinished(task_id.max(0) as u64, &label, None)
            })
            .collect::<Vec<_>>();
        if !rows.is_empty() {
            self.client
                .execute(
                    "DELETE FROM running_tasks \
                     WHERE workspace_id = $1 AND subagent_session_id = $2",
                    &[&self.workspace_id, &subagent_session_id],
                )
                .await
                .context("cannot clear unfinished subagent tasks")?;
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

/// Group raw DB entries by seq, keeping only the row(s) with the latest
/// `event_time` per seq.
///
/// - Older event_time rows for the same seq are silently discarded
///   (they are considered overwritten by the newer write).
/// - If the latest event_time has multiple physical rows (same microsecond),
///   their deserialised payloads are compared.  All identical → folded;
///   any divergent → `Err` with diagnostic guidance.
/// - Output is sorted by the winning event_time ASC, then seq ASC.
///
/// Exported as `pub(crate)` for testing without a live DB connection.
pub(crate) fn dedup_raw_entries(
    raw: &[(i64, chrono::NaiveDateTime, String)],
    session_id: &str,
    workspace_id: &str,
) -> Result<Vec<(i64, SessionEntry)>> {
    // Group by seq: keep max event_time and all payloads at that max.
    let mut per_seq: std::collections::HashMap<i64, (chrono::NaiveDateTime, Vec<String>)> =
        std::collections::HashMap::with_capacity(raw.len().min(64));

    for (seq, et, payload) in raw {
        let entry = per_seq.entry(*seq).or_insert((*et, Vec::new()));
        match (*et).cmp(&entry.0) {
            std::cmp::Ordering::Greater => {
                // Newer event_time → overwrite older entries entirely.
                entry.0 = *et;
                entry.1 = vec![payload.clone()];
            }
            std::cmp::Ordering::Equal => {
                // Same max event_time → accumulate for conflict check.
                entry.1.push(payload.clone());
            }
            std::cmp::Ordering::Less => {
                // Older event_time → silently discard.
            }
        }
    }

    let mut entries = Vec::with_capacity(per_seq.len());
    for (seq, (max_et, payloads)) in per_seq {
        let first_entry: SessionEntry = serde_json::from_str(&payloads[0]).with_context(|| {
            format!(
                "cannot decode session {} (seq {} event_time {})",
                session_id, seq, max_et
            )
        })?;

        for (i, payload) in payloads.iter().enumerate().skip(1) {
            let other: SessionEntry = serde_json::from_str(payload).with_context(|| {
                format!(
                    "cannot decode session {} seq {} max_event_time {} (dup {})",
                    session_id, seq, max_et, i
                )
            })?;
            if other != first_entry {
                anyhow::bail!(
                    "session '{}' seq {} event_time {} has divergent physical \
                     duplicates; cannot safely load. Stop writers, inspect with SQL:\n\
                     SELECT * FROM session_entries \
                     WHERE workspace_id = '{}' AND session_id = '{}' AND seq = {} \
                     ORDER BY event_time;\n\
                     Resolve manually (new session or repair) then re-run import.",
                    session_id,
                    seq,
                    max_et,
                    workspace_id,
                    session_id,
                    seq,
                );
            }
        }

        entries.push((max_et, seq, first_entry));
    }

    entries.sort_unstable_by_key(|(event_time, seq, _)| (*event_time, *seq));
    Ok(entries
        .into_iter()
        .map(|(_, seq, entry)| (seq, entry))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AssistantMessage;

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
            }
            .into(),
            Message::Tool {
                call_id: "call_err".into(),
                name: "bash".into(),
                content: "command not found".into(),
                is_error: true,
                synthetic: false,
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
            },
            SessionEntry::Notice {
                text: "[background task 1 completed]\nexit: 0".into(),
            },
            SessionEntry::BackgroundCompletion {
                id: 2,
                output: "exit code: 0\nstdout:\nbuilt successfully\nstderr:\n".into(),
                label: None,
            },
            // Entry with a label to verify serde roundtrip with label.
            SessionEntry::BackgroundCompletion {
                id: 3,
                output: "some output".into(),
                label: Some("build project".into()),
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
        let mut session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        let entries = test_entries();
        session.append(&entries).await.unwrap();

        let loaded = session.load().await.unwrap();
        assert_eq!(loaded.len(), entries.len());
        for (got, want) in loaded.iter().zip(entries.iter()) {
            assert_eq!(got, want);
        }
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

        let mut s1 = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        s1.append(&entries[..3]).await.unwrap();
        drop(s1);

        let mut s2 = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
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
        let mut session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        // First write: seq 0..3
        session.append(&entries[..3]).await.unwrap();
        // Simulate reconnect-retry: same seq range, but new event_times
        session.next_seq = 0;
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
    async fn retry_different_payload_newer_wins() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-payload-{}", crate::session::new_id());
        let mut session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

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
        // Retry same seq 0 with different payload, newer event_time
        session.next_seq = 0;
        session
            .append(std::slice::from_ref(&new_entry))
            .await
            .unwrap();

        let loaded = session.load().await.unwrap();
        assert_eq!(
            loaded.len(),
            1,
            "retry with different payload: latest event_time wins (expected 1, got {})",
            loaded.len(),
        );
        assert_eq!(
            loaded[0], new_entry,
            "newer event_time entry should win over older"
        );
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
        let mut writer_a = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        writer_a.append(&entries[..3]).await.unwrap();
        drop(writer_a);

        // Writer B: connect (reads max_seq=2, sets next_seq=3).
        let mut writer_b = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        // Simulate concurrent writer A' appending seq 3..4 between connect and load.
        let mut writer_a2 = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
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
            writer_b.next_seq, 5,
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
        let mut session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
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

        let mut sa = GreptimeSession::connect(&conn, &wid_a, &sid).await.unwrap();
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

    #[tokio::test]
    async fn load_head_and_load_older_segmented() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-seg-{}", crate::session::new_id());
        let mut session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

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
        let mut session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

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

    #[tokio::test]
    async fn load_oldest_and_load_newer_segmented() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-newer-{}", crate::session::new_id());
        let mut session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

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
        let mut no_comp = GreptimeSession::connect(&conn, &wid, &sid_none)
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
        let result = dedup_raw_entries(&raw, "test-session", "test-workspace").unwrap();
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
        let result = dedup_raw_entries(&raw, "test-session", "test-workspace").unwrap();
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
        let result = dedup_raw_entries(&raw, "test-session", "test-workspace").unwrap();
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
        let result = dedup_raw_entries(&raw, "test-session", "test-workspace").unwrap();
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
        let result = dedup_raw_entries(&raw, "test-session", "test-workspace").unwrap();
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
        let err = dedup_raw_entries(&raw, "test-session", "test-workspace").unwrap_err();
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
        let err = dedup_raw_entries(&raw, "test-session", "test-workspace").unwrap_err();
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
        let result = dedup_raw_entries(&raw, "test-session", "test-workspace").unwrap();
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
        let result = dedup_raw_entries(&raw, "test-session", "test-workspace").unwrap();
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
        let result = dedup_raw_entries(&raw, "test-session", "test-workspace").unwrap();
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
        let result = dedup_raw_entries(&[], "test-session", "test-workspace").unwrap();
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
        let result = dedup_raw_entries(&raw, "test-session", "test-workspace").unwrap();
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
        assert_eq!(s.next_seq, 3);

        // Normal advance: 3 → 10
        s.advance_next_seq_from_snapshot_len(10).unwrap();
        assert_eq!(s.next_seq, 10);

        // Same value is allowed (no-op advance).
        s.advance_next_seq_from_snapshot_len(10).unwrap();
        assert_eq!(s.next_seq, 10);
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
            .record_task_start(&sid, 1, "sleep 100", None)
            .await
            .unwrap();
        session
            .record_task_start(&sid, 2, "cargo build", Some("sub-probe"))
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
            .record_task_start(&sid, 1, "restarted", None)
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
            .record_task_start(&parent_a, 10, "probe a", Some(&subagent))
            .await
            .unwrap();
        session
            .record_task_start(&parent_b, 11, "probe b", Some(&subagent))
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
            .create_meta(&sid, Some("model-x"), Some("main"), None, None)
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

        // create is idempotent: a second create appends nothing.
        session
            .create_meta(&sid, Some("model-x"), Some("main"), None, None)
            .await
            .unwrap();
        assert_eq!(
            session.audit_meta(&sid).await.unwrap().len(),
            1,
            "re-create must not append a second creation snapshot"
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

    #[tokio::test]
    async fn sessions_meta_entry_count_is_next_seq() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-meta-seq-{}", crate::session::new_id());
        let mut session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        let entries = test_entries();

        // entry_count = next_seq (MAX(seq)+1 at connect, advanced by
        // append) — not a physical row count.
        session.append(&entries[..3]).await.unwrap();
        assert_eq!(session.next_seq, 3);
        session
            .create_meta(&sid, Some("m"), None, None, None)
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
        assert_eq!(session.next_seq, 5);
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
        let mut session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        let entries = test_entries();

        // Pre-table session: transcript rows, no metadata rows.
        session.append(&entries[..3]).await.unwrap();
        assert!(session.audit_meta(&sid).await.unwrap().is_empty());

        session.backfill_sessions().await.unwrap();
        let list = session.list_meta().await.unwrap();
        assert_eq!(list.len(), 1, "only this session in the isolated workspace");
        let meta = &list[0];
        assert_eq!(meta.session_id, sid);
        assert_eq!(meta.entry_count, 3, "MAX(seq)+1, not COUNT");
        assert_eq!(meta.model, None, "pre-table sessions have no model");

        // Idempotent: a second run inserts nothing; the list is identical.
        session.backfill_sessions().await.unwrap();
        let list = session.list_meta().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].entry_count, 3);
        assert_eq!(list[0].created_at, meta.created_at);
        assert_eq!(list[0].last_active_at, meta.last_active_at);
        assert_eq!(
            session.audit_meta(&sid).await.unwrap().len(),
            1,
            "second backfill appends nothing"
        );
    }
}
