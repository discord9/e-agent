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
}
