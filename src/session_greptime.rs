//! GreptimeDB-backed session storage. Optional runtime-selectable
//! backend via `[session] backend = "greptime"`. Still experimental.
//! Uses tokio-postgres for both read and write against the
//! `session_entries` table.
//!
//! Non-goals: no Storage trait, no migration of existing JSONL sessions,
//! no background-task bookkeeping (that's still JSONL).

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

        // Find the current max seq via LastRow scan (reads ~2 rows instead of
        // scanning the entire session). Falls back to -1 for new sessions.
        let row = client
            .query_opt(
                "SELECT last_value(seq ORDER BY event_time ASC) AS last_seq \
                 FROM session_entries \
                 WHERE workspace_id = $1 AND session_id = $2 \
                 GROUP BY workspace_id, session_id",
                &[&workspace_id, &session_id],
            )
            .await
            .context("cannot query last seq")?;
        let max_seq: i64 = row.map_or(-1, |r| r.get("last_seq"));

        Ok(Self {
            client,
            next_seq: max_seq + 1,
            workspace_id: workspace_id.to_string(),
            session_id: session_id.to_string(),
        })
    }

    /// Load all entries for this session. Queries by TIME INDEX (DESC) and
    /// deduplicates by seq in application code — first occurrence wins (which
    /// is the latest write for that seq, since we're scanning newest-first).
    pub async fn load(&self) -> Result<Vec<SessionEntry>> {
        let rows = self
            .client
            .query(
                "SELECT seq, payload FROM session_entries \
                 WHERE workspace_id = $1 AND session_id = $2 \
                 ORDER BY event_time DESC",
                &[&self.workspace_id, &self.session_id],
            )
            .await
            .context("cannot load session entries")?;

        // Dedup: keep first occurrence per seq (latest write, since DESC).
        let mut seen = std::collections::HashMap::with_capacity(rows.len());
        for row in &rows {
            let seq: i64 = row.get("seq");
            let payload: &str = row.get("payload");
            seen.entry(seq).or_insert(payload);
        }

        // Sort by seq ascending and deserialize.
        let mut pairs: Vec<_> = seen.into_iter().collect();
        pairs.sort_by_key(|(seq, _)| *seq);

        let mut entries = Vec::with_capacity(pairs.len());
        for (seq, payload) in pairs {
            let entry: SessionEntry = serde_json::from_str(payload)
                .with_context(|| format!("cannot decode session {} seq {seq}", self.session_id))?;
            entries.push(entry);
        }
        Ok(entries)
    }

    /// Append new entries atomically per multi-row INSERT statement.
    ///
    /// GreptimeDB's pg-wire does not support transactions, so atomicity
    /// is per statement: a single multi-row INSERT either commits all N rows
    /// or commits zero. Serialization happens before any DB write.
    ///
    /// Chunking: tokio-postgres / pg-wire allows at most 65535 bound
    /// parameters, so entries are chunked at 10000 per statement
    /// (70000 params). >10000-entry appends can partially commit across
    /// chunks; acceptable, turns are far smaller.
    ///
    /// `next_seq` is computed once before any chunk loop. On failure
    /// `next_seq` is unchanged, so retries of the same slice reuse the same
    /// seq range. Duplicates of a fully-committed-then-retried batch are
    /// handled by read-side dedup (equal seq, first-wins by event_time DESC).
    pub async fn append(&mut self, entries: &[SessionEntry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let n = entries.len();
        const CHUNK_SIZE: usize = 10000;

        // Serialize all entries upfront so serialization errors happen
        // before any database write.
        let mut prepped: Vec<(i64, chrono::NaiveDateTime, String, String, bool)> =
            Vec::with_capacity(n);
        // Compute base seq once — all chunks share the same range so retry
        // of a partially-committed chunk does not shift seqs.
        let base_seq = self.next_seq;
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
            let mut params: Vec<Box<dyn tokio_postgres::types::ToSql + Sync>> =
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

            let param_refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                params.iter().map(|p| p.as_ref()).collect();
            self.client
                .execute(&sql, &param_refs)
                .await
                .context("cannot append chunk to session_entries")?;
        }

        // Advance next_seq only after all chunks succeed, so a partial
        // failure does not shift sequence numbers on retry.
        self.next_seq = base_seq + entries.len() as i64;

        Ok(())
    }
}

/// Build a multi-row INSERT with 7 bound parameters per row (workspace_id, session_id, seq, event_time, entry_kind, payload, schema_version, is_error).
/// Example (1 row):
/// `INSERT INTO session_entries (workspace_id, session_id, seq, event_time, entry_kind, payload, schema_version, is_error) VALUES ($1,$2,$3,$4,$5,$6,1,$7)`
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
            }
            .into(),
            Message::Tool {
                call_id: "call_err".into(),
                name: "bash".into(),
                content: "command not found".into(),
                is_error: true,
            }
            .into(),
            SessionEntry::Compaction {
                summary: "## summary text".into(),
                retained: vec![
                    Message::User {
                        content: "retained user".into(),
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
            },
            Message::User {
                content: "你好世界👋\n多行".into(),
            }
            .into(),
            Message::User {
                content: "'); DROP TABLE session_entries; --".into(),
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
    async fn duplicate_retry_dedup() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-dup-{}", crate::session::new_id());
        let entries = test_entries();
        let mut session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();

        // Write twice (simulating retry)
        session.append(&entries[..3]).await.unwrap();
        // Reset seq to simulate reconnect-retry
        session.next_seq = 0;
        session.append(&entries[..3]).await.unwrap();

        let loaded = session.load().await.unwrap();
        assert_eq!(loaded.len(), 3);
        for (got, want) in loaded.iter().zip(entries[..3].iter()) {
            assert_eq!(got, want);
        }
    }

    #[tokio::test]
    async fn append_is_atomic_per_call() {
        let conn = conn_str();
        if conn == "skipped" {
            eprintln!("skipping: GREPTIME_PG not set");
            return;
        }
        let wid = workspace_id();
        let sid = format!("test-gt-atomic-{}", crate::session::new_id());
        let mut session = GreptimeSession::connect(&conn, &wid, &sid).await.unwrap();
        let entries = test_entries();

        // Append a slice of 5 entries in one call; verify all 5 recovered.
        session.append(&entries[..5]).await.unwrap();
        let loaded = session.load().await.unwrap();
        assert_eq!(loaded.len(), 5);
        for (got, want) in loaded.iter().zip(entries[..5].iter()) {
            assert_eq!(got, want);
        }

        // Append 2 more; verify seqs remain contiguous (0..7, no gaps).
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

    #[test]
    fn workspace_id_is_stable() {
        let a = derive_workspace_id(Path::new("/tmp/ws"));
        let b = derive_workspace_id(Path::new("/tmp/ws"));
        assert_eq!(a, b);
    }
}
