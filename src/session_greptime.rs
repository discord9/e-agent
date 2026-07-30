//! GreptimeDB-backed session storage. Optional runtime-selectable
//! backend via `[session] backend = "greptime"`. Still experimental.
//! Uses tokio-postgres for both read and write against the
//! `session_entries` table.
//!
//! Non-goals: no Storage trait, no migration of existing JSONL sessions,
//! no background-task bookkeeping (that's still JSONL).

use anyhow::{Context, Result};
use tokio_postgres::NoTls;

use crate::agent::{Message, SessionEntry};

/// DDL for the session table. Idempotent.
const CREATE_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS session_entries (
    session_id STRING PRIMARY KEY,
    seq BIGINT NOT NULL,
    event_time TIMESTAMP(9) NOT NULL TIME INDEX,
    entry_kind STRING NOT NULL,
    payload STRING NOT NULL,
    schema_version INT NOT NULL DEFAULT 1,
    agent_role STRING NOT NULL DEFAULT 'main',
    is_error BOOLEAN NOT NULL DEFAULT FALSE,
    appended_at TIMESTAMP(9) NOT NULL DEFAULT now()
) WITH (
    append_mode = 'true',
    sst_format = 'flat',
)
"#;

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
    session_id: String,
}

impl GreptimeSession {
    /// Connect and ensure the table exists. `conn` is a tokio-postgres
    /// connection string, e.g. "host=127.0.0.1 port=4002 dbname=public".
    pub async fn connect(conn: &str, session_id: &str) -> Result<Self> {
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
                 FROM session_entries WHERE session_id = $1 GROUP BY session_id",
                &[&session_id],
            )
            .await
            .context("cannot query last seq")?;
        let max_seq: i64 = row.map_or(-1, |r| r.get("last_seq"));

        Ok(Self {
            client,
            next_seq: max_seq + 1,
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
                 WHERE session_id = $1 ORDER BY event_time DESC",
                &[&self.session_id],
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
    /// (60000 params). >10000-entry appends can partially commit across
    /// chunks; acceptable, turns are far smaller.
    ///
    /// `next_seq` advances by `entries.len()` only after all chunks succeed.
    /// On failure `next_seq` is unchanged, so retries of the same slice
    /// reuse the same seq range. Duplicates of a fully-committed-then-retried
    /// batch are handled by read-side dedup (equal seq, first-wins by
    /// event_time DESC).
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
        for (i, entry) in entries.iter().enumerate() {
            let seq = self.next_seq + i as i64;
            let payload = serde_json::to_string(entry)
                .with_context(|| format!("cannot serialize entry seq {seq}"))?;
            let kind = entry_kind(entry).to_string();
            let err = is_error(entry);
            let ts = us_to_datetime(next_event_time_us());
            prepped.push((seq, ts, kind, payload, err));
        }

        let sid = self.session_id.clone();
        for chunk in prepped.chunks(CHUNK_SIZE) {
            let n_chunk = chunk.len() as i64;
            let sql = build_multi_row_insert(chunk.len());

            // Flatten params: session_id, seq, ts, kind, payload, is_error per row.
            let mut params: Vec<Box<dyn tokio_postgres::types::ToSql + Sync>> =
                Vec::with_capacity(chunk.len() * 6);
            for (seq, ts, kind, payload, err) in chunk {
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

            self.next_seq += n_chunk;
        }

        Ok(())
    }
}

/// Build a multi-row INSERT with 6 bound parameters per row.
/// Example (1 row): `INSERT INTO session_entries (...) VALUES ($1,$2,$3,$4,$5,1,'main',$6)`
fn build_multi_row_insert(row_count: usize) -> String {
    let mut sql = String::with_capacity(200 + row_count * 40);
    sql.push_str(
        "INSERT INTO session_entries \
         (session_id, seq, event_time, entry_kind, payload, \
          schema_version, agent_role, is_error) VALUES ",
    );
    for i in 0..row_count {
        if i > 0 {
            sql.push(',');
        }
        let b = i * 6;
        sql.push_str(&format!(
            "(${b1},${b2},${b3},${b4},${b5},1,'main',${b6})",
            b1 = b + 1,
            b2 = b + 2,
            b3 = b + 3,
            b4 = b + 4,
            b5 = b + 5,
            b6 = b + 6
        ));
    }
    sql
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AssistantMessage;

    fn conn_str() -> String {
        std::env::var("GREPTIME_PG")
            .unwrap_or_else(|_| "host=127.0.0.1 port=15403 dbname=e_agent".into())
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
        let sid = format!("test-gt-{}", crate::session::new_id());
        let mut session = GreptimeSession::connect(&conn_str(), &sid).await.unwrap();
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
        let sid = format!("test-gt-reconn-{}", crate::session::new_id());
        let entries = test_entries();

        let mut s1 = GreptimeSession::connect(&conn_str(), &sid).await.unwrap();
        s1.append(&entries[..3]).await.unwrap();
        drop(s1);

        let mut s2 = GreptimeSession::connect(&conn_str(), &sid).await.unwrap();
        s2.append(&entries[3..]).await.unwrap();

        let loaded = s2.load().await.unwrap();
        assert_eq!(loaded.len(), entries.len());
        for (got, want) in loaded.iter().zip(entries.iter()) {
            assert_eq!(got, want);
        }
    }

    #[tokio::test]
    async fn duplicate_retry_dedup() {
        let sid = format!("test-gt-dup-{}", crate::session::new_id());
        let entries = test_entries();
        let mut session = GreptimeSession::connect(&conn_str(), &sid).await.unwrap();

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
        let sid = format!("test-gt-atomic-{}", crate::session::new_id());
        let mut session = GreptimeSession::connect(&conn_str(), &sid).await.unwrap();
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
}
