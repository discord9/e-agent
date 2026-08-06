//! SQLite/turso backend tests. All runnable on Linux WITHOUT a live
//! database: `:memory:` for tests that want a fresh per-connection
//! database, tempfile tempdirs for file-backed tests that need to
//! reconnect (each test gets its own file, so no cross-test pollution).

use super::*;
use crate::agent::{AssistantMessage, Message};
use crate::session_store::SessionMeta;
use std::path::{Path, PathBuf};

fn workspace_id() -> String {
    derive_workspace_id(Path::new("/tmp/e-agent-test-sqlite"))
}

fn temp_db() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sessions.db");
    (dir, path)
}

/// Connect a session bound to a fresh unique session id on a fresh
/// tempdir-backed database file.
async fn fresh_session() -> (tempfile::TempDir, SqliteSession, String) {
    let (dir, path) = temp_db();
    let sid = format!("test-sql-{}", crate::session::new_id());
    let session = SqliteSession::connect(path.to_str().unwrap(), &workspace_id(), &sid)
        .await
        .expect("connect");
    (dir, session, sid)
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

// ----------------------------------------------------------------------
// connect / table creation
// ----------------------------------------------------------------------

#[tokio::test]
async fn connect_memory_creates_tables_idempotently() {
    // `:memory:`: every connect is a fresh database, so this exercises the
    // CREATE TABLE IF NOT EXISTS path on an empty database twice.
    let wid = workspace_id();
    let sid = format!("test-sql-mem-{}", crate::session::new_id());
    let s1 = SqliteSession::connect(":memory:", &wid, &sid)
        .await
        .unwrap();
    let s2 = SqliteSession::connect(":memory:", &wid, &sid)
        .await
        .unwrap();
    // Both connect cleanly and can round-trip entries.
    let entry = Message::User {
        content: "mem".into(),
        images: vec![],
    }
    .into();
    s1.append(&[entry]).await.unwrap();
    assert_eq!(s2.load().await.unwrap().len(), 0, "separate memory DBs");
}

#[tokio::test]
async fn connect_creates_missing_parent_dir() {
    // db_path points into a directory chain that does not exist yet:
    // connect must create the parent directories instead of failing
    // (the Windows `D:/.e-agent/sessions.db` case).
    let base = tempfile::tempdir().expect("tempdir");
    let path = base
        .path()
        .join("nested")
        .join("deeper")
        .join("sessions.db");
    assert!(
        !path.parent().unwrap().exists(),
        "precondition: no parent dir"
    );
    let wid = workspace_id();
    let sid = format!("test-sql-mkdir-{}", crate::session::new_id());

    let session = SqliteSession::connect(path.to_str().unwrap(), &wid, &sid)
        .await
        .expect("connect creates missing parent dirs");
    assert!(
        path.parent().unwrap().is_dir(),
        "parent directory was created: {}",
        path.parent().unwrap().display()
    );

    // The created database is fully usable, and a second workspace sharing
    // the same file (create_dir_all must be idempotent) connects too.
    let entry = Message::User {
        content: "mkdir".into(),
        images: vec![],
    }
    .into();
    session.append(&[entry]).await.unwrap();
    let shared = SqliteSession::connect(path.to_str().unwrap(), &workspace_id(), &sid)
        .await
        .expect("reconnect with existing parent dir");
    assert_eq!(shared.load().await.unwrap().len(), 1);
    drop((base, session, shared));
}

#[tokio::test]
async fn connect_memory_ignores_missing_parent_dir() {
    // `:memory:` has no filesystem parent — the parent-dir creation must
    // be skipped, not attempted against an empty path.
    let wid = workspace_id();
    let sid = format!("test-sql-mem-mkdir-{}", crate::session::new_id());
    let session = SqliteSession::connect(":memory:", &wid, &sid)
        .await
        .expect(":memory: connect unaffected by parent-dir creation");
    let entry = Message::User {
        content: "mem-mkdir".into(),
        images: vec![],
    }
    .into();
    session.append(&[entry]).await.unwrap();
    assert_eq!(session.load().await.unwrap().len(), 1);
}

#[tokio::test]
async fn connect_twice_on_file_is_idempotent_and_persists() {
    let (dir, path) = temp_db();
    let wid = workspace_id();
    let sid = format!("test-sql-twice-{}", crate::session::new_id());
    let p = path.to_str().unwrap();

    let entries = test_entries();
    let s1 = SqliteSession::connect(p, &wid, &sid).await.unwrap();
    s1.append(&entries[..3]).await.unwrap();
    s1.create_meta(&sid, Some("m"), Some("main"), None, None, None)
        .await
        .unwrap();
    drop(s1);

    // Second connect over the same file: tables already exist (idempotent
    // DDL) and both transcript + metadata survive.
    let s2 = SqliteSession::connect(p, &wid, &sid).await.unwrap();
    let loaded = s2.load().await.unwrap();
    assert_eq!(loaded.len(), 3);
    for (got, want) in loaded.iter().zip(entries[..3].iter()) {
        assert_eq!(got, want);
    }
    let list = s2.list_meta().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].session_id, sid);
    drop((dir, s2));
}

/// 老库迁移：一个在 archive/title/pinned/writer 特性之前创建的 sessions
/// 表（没有这四个列）在 connect 时自动 ALTER 补列，随后共享的
/// archived/pinned/title/writer 列查询与写入全部正常 —— 覆盖
/// "老库无 archived 列 → connect 后 SELECT 正常"（pinned 当初同样缺列，
/// 只是 archive 是新加列，老库文件必然触发）。
#[tokio::test]
async fn connect_migrates_legacy_sessions_table_missing_feature_columns() {
    let (dir, path) = temp_db();
    let wid = workspace_id();
    let p = path.to_str().unwrap();

    // 手工建一个「旧版」sessions 表：只有基础列，没有 title/pinned/
    // archived/writer，并写入一条旧元数据行（时间戳为微秒）。
    let legacy_ddl = r#"
        CREATE TABLE sessions (
            workspace_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            last_active_at INTEGER NOT NULL,
            model TEXT NULL,
            "role" TEXT NULL,
            entry_count INTEGER NOT NULL DEFAULT 0,
            parent_session_id TEXT NULL,
            parent_task_id INTEGER NULL
        )
    "#;
    {
        let db = turso::Builder::new_local(p)
            .build()
            .await
            .expect("open legacy db");
        let conn = db.connect().expect("connect legacy db");
        conn.execute(legacy_ddl, ())
            .await
            .expect("create legacy sessions table");
        conn.execute(
            "INSERT INTO sessions \
             (workspace_id, session_id, created_at, last_active_at, entry_count) \
             VALUES (?1, ?2, ?3, ?4, 2)",
            (
                wid.as_str(),
                "legacy-1",
                1_700_000_000_000_000i64,
                1_700_000_000_100_000i64,
            ),
        )
        .await
        .expect("insert legacy meta row");
    }

    // connect 应自动探测 + ALTER：老行读回 archived/pinned/title = NULL，
    // 写入路径（archive 一个老会话）落新列，list_meta 的共享 SELECT 正常。
    let session = SqliteSession::connect(p, &wid, "legacy-1")
        .await
        .expect("connect migrates the legacy table");
    let meta = session
        .load_meta_row("legacy-1")
        .await
        .expect("query migrated meta")
        .expect("legacy row readable after migration");
    assert_eq!(meta.session_id, "legacy-1");
    assert_eq!(meta.entry_count, 2);
    assert_eq!(meta.archived, None, "old rows read archived back as NULL");
    assert_eq!(meta.pinned, None, "old rows read pinned back as NULL");
    assert_eq!(meta.title, None, "old rows read title back as NULL");
    assert_eq!(meta.writer, None, "old rows read writer back as NULL");

    session
        .set_archived("legacy-1", true)
        .await
        .expect("archive legacy session");
    let meta = session
        .load_meta_row("legacy-1")
        .await
        .expect("query after archive")
        .expect("row after archive");
    assert_eq!(
        meta.archived,
        Some(true),
        "new snapshot row carries archived"
    );
    let list = session
        .list_meta()
        .await
        .expect("list_meta after migration");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].archived, Some(true));

    // 迁移幂等：再 connect 一次，探测发现列已存在、不重复 ALTER，一切正常。
    drop(session);
    let session2 = SqliteSession::connect(p, &wid, "legacy-1")
        .await
        .expect("reconnect on migrated db");
    let list = session2
        .list_meta()
        .await
        .expect("list_meta after reconnect");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].archived, Some(true));
    drop((dir, session2));
}

// ----------------------------------------------------------------------
// append / load round-trips
// ----------------------------------------------------------------------

#[tokio::test]
async fn append_load_roundtrip_preserves_entries_and_order() {
    let (_dir, session, _sid) = fresh_session().await;
    let entries = test_entries();
    session.append(&entries).await.unwrap();

    let loaded = session.load().await.unwrap();
    assert_eq!(loaded.len(), entries.len());
    for (got, want) in loaded.iter().zip(entries.iter()) {
        assert_eq!(got, want);
    }

    // load_with_seq exposes the contiguous seq assignment 0..N.
    let with_seq = session.load_with_seq().await.unwrap();
    assert_eq!(with_seq.len(), entries.len());
    for (i, (seq, entry)) in with_seq.iter().enumerate() {
        assert_eq!(*seq, i as i64);
        assert_eq!(entry, &entries[i]);
    }
}

#[tokio::test]
async fn append_after_reconnect() {
    let (_dir, path) = temp_db();
    let wid = workspace_id();
    let sid = format!("test-sql-reconn-{}", crate::session::new_id());
    let p = path.to_str().unwrap();
    let entries = test_entries();

    let s1 = SqliteSession::connect(p, &wid, &sid).await.unwrap();
    s1.append(&entries[..3]).await.unwrap();
    drop(s1);

    let s2 = SqliteSession::connect(p, &wid, &sid).await.unwrap();
    s2.append(&entries[3..]).await.unwrap();

    let loaded = s2.load().await.unwrap();
    assert_eq!(loaded.len(), entries.len());
    for (got, want) in loaded.iter().zip(entries.iter()) {
        assert_eq!(got, want);
    }
}

#[tokio::test]
async fn append_appends_with_contiguous_seq() {
    let (_dir, session, _sid) = fresh_session().await;
    let entries = test_entries();

    session.append(&entries[..5]).await.unwrap();
    let loaded = session.load().await.unwrap();
    assert_eq!(loaded.len(), 5);
    for (got, want) in loaded.iter().zip(entries[..5].iter()) {
        assert_eq!(got, want);
    }

    session.append(&entries[5..7]).await.unwrap();
    let loaded = session.load().await.unwrap();
    assert_eq!(loaded.len(), 7);
    for (got, want) in loaded.iter().zip(entries[..7].iter()) {
        assert_eq!(got, want);
    }
}

#[tokio::test]
async fn same_session_name_different_workspaces_are_isolated() {
    let (_dir, path) = temp_db();
    let wid_a = derive_workspace_id(Path::new("/tmp/e-agent-workspace-a"));
    let wid_b = derive_workspace_id(Path::new("/tmp/e-agent-workspace-b"));
    let sid = format!("shared-session-name-{}", crate::session::new_id());
    let p = path.to_str().unwrap();

    let sa = SqliteSession::connect(p, &wid_a, &sid).await.unwrap();
    let sb = SqliteSession::connect(p, &wid_b, &sid).await.unwrap();

    let user_msg = Message::User {
        content: "only in workspace A".into(),
        images: vec![],
    };
    sa.append(&[user_msg.clone().into()]).await.unwrap();

    // Workspace B must not see workspace A's entries despite using the
    // same session name in the same database file.
    let loaded_b = sb.load().await.unwrap();
    assert!(loaded_b.is_empty(), "workspace B must not see A's entries");

    let loaded_a = sa.load().await.unwrap();
    assert_eq!(loaded_a.len(), 1);
}

// ----------------------------------------------------------------------
// per-seq dedup: latest event_time wins; divergent same-time hard-errors
// ----------------------------------------------------------------------

#[tokio::test]
async fn retry_latest_event_time_wins_per_seq() {
    let (_dir, session, _sid) = fresh_session().await;
    let entries = test_entries();

    // First write: seq 0..3.
    session.append(&entries[..3]).await.unwrap();

    // Simulate reconnect-retry: same seq range, but strictly newer
    // event_times. The SQLite `append` path treats an identical-payload
    // retry as its own earlier commit and folds it without writing a new
    // row (`append_reuses_own_retry`), so the retried rows are landed
    // directly — exactly what a reconnecting writer produces in Greptime's
    // append mode (and what a migrated/repaired database would contain).
    let newer: Vec<SessionEntry> = (0..3)
        .map(|i| {
            Message::User {
                content: format!("retried content {i}"),
                images: vec![],
            }
            .into()
        })
        .collect();
    let conn = session.conn.lock().await;
    for (i, entry) in newer.iter().enumerate() {
        conn.execute(
            "INSERT INTO session_entries \
             (workspace_id, session_id, seq, event_time_us, entry_kind, payload, \
              schema_version, is_error) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, 0)",
            (
                session.workspace_id.as_str(),
                session.session_id.as_str(),
                i as i64,
                next_event_time_us(),
                entry_kind(entry),
                serde_json::to_string(entry).unwrap(),
            ),
        )
        .await
        .unwrap();
    }
    drop(conn);

    // With per-seq dedup (group by seq, latest event_time wins), each
    // retried seq keeps only the newer write → 3 entries carrying the
    // newer payload; the older rows are folded away, not merged.
    let loaded = session.load().await.unwrap();
    assert_eq!(loaded.len(), 3, "retry: latest per seq wins");
    for (got, want) in loaded.iter().zip(newer.iter()) {
        assert_eq!(got, want, "retry content mismatch: newer payload must win");
    }

    // Both versions are physically stored (a new row with a larger
    // event_time_us really was created per seq); the loader folded the
    // older one away instead of deleting it.
    let conn = session.conn.lock().await;
    let mut rows = conn
        .query(
            "SELECT COUNT(*) FROM session_entries \
             WHERE workspace_id = ?1 AND session_id = ?2",
            (session.workspace_id.as_str(), session.session_id.as_str()),
        )
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    assert_eq!(
        row.get_value(0).unwrap().as_integer().copied(),
        Some(6),
        "retry must create a newer physical row per seq, not fold it"
    );
    drop(conn);
}

#[tokio::test]
async fn retry_different_payload_rejected_as_conflict() {
    let (_dir, session, _sid) = fresh_session().await;
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
    *session.next_seq.lock().unwrap() = 0;
    let err = session
        .append(std::slice::from_ref(&new_entry))
        .await
        .unwrap_err();
    assert!(err.contains("concurrent write conflict"), "got: {err}");

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
async fn append_failed_mid_statement_commits_nothing() {
    let (_dir, session, _sid) = fresh_session().await;

    // Force a mid-statement constraint failure on a LATER row of the
    // batch: a UNIQUE index on payload makes the fourth row collide with
    // the third, aborting the single multi-row INSERT after the first
    // three rows would otherwise have been written. Statement atomicity
    // must leave the earlier rows of the same append absent.
    let conn = session.conn.lock().await;
    conn.execute(
        "CREATE UNIQUE INDEX idx_payload_uniq ON session_entries(payload)",
        (),
    )
    .await
    .unwrap();
    drop(conn);

    let batch = [
        Message::User {
            content: "first ok".into(),
            images: vec![],
        }
        .into(),
        Message::User {
            content: "second ok".into(),
            images: vec![],
        }
        .into(),
        Message::User {
            content: "third ok".into(),
            images: vec![],
        }
        .into(),
        // Same payload as the third row → UNIQUE violation on the last row.
        Message::User {
            content: "third ok".into(),
            images: vec![],
        }
        .into(),
    ];
    let err = session.append(&batch).await.unwrap_err();
    assert!(
        err.contains("cannot append"),
        "mid-statement failure surfaces as an append error: {err}"
    );

    // Atomicity: the earlier rows of the same append are absent too.
    let loaded = session.load().await.unwrap();
    assert!(
        loaded.is_empty(),
        "failed append must not commit earlier rows of the same batch"
    );
    assert_eq!(
        *session.next_seq.lock().unwrap(),
        0,
        "cursor must not advance when the append fails"
    );
}

#[tokio::test]
async fn append_stored_event_times_are_strictly_monotonic() {
    let (_dir, session, _sid) = fresh_session().await;
    let entries = test_entries();

    // Two appends with distinct event times. `next_event_time_us` is
    // strictly monotonic per process, so every row of the second append
    // must be STORED with a strictly larger event_time_us than every row
    // of the first — asserted with a raw query, since the loader's dedup
    // could mask timestamp ordering.
    session.append(&entries[..2]).await.unwrap();
    session.append(&entries[2..4]).await.unwrap();

    let conn = session.conn.lock().await;
    let mut rows = conn
        .query(
            "SELECT seq, event_time_us FROM session_entries \
             WHERE workspace_id = ?1 AND session_id = ?2 \
             ORDER BY seq ASC",
            (session.workspace_id.as_str(), session.session_id.as_str()),
        )
        .await
        .unwrap();
    let mut times = Vec::new();
    while let Some(row) = rows.next().await.unwrap() {
        times.push(
            row.get_value(1)
                .unwrap()
                .as_integer()
                .copied()
                .expect("event_time_us is an integer"),
        );
    }
    drop(conn);

    assert_eq!(times.len(), 4, "two appends × two rows");
    assert!(times[0] < times[1], "within-append times advance");
    assert!(times[2] < times[3], "within-append times advance");
    assert!(
        times[2] > times[1],
        "second append's stored event_time_us must be strictly larger than the first's"
    );
}

#[tokio::test]
async fn append_detects_concurrent_writer() {
    let (_dir, path) = temp_db();
    let wid = workspace_id();
    let sid = format!("test-sql-conflict-{}", crate::session::new_id());
    let p = path.to_str().unwrap();
    let entries = test_entries();

    // Writer A: writes seqs 0..2.
    let writer_a = SqliteSession::connect(p, &wid, &sid).await.unwrap();
    writer_a.append(&entries[..2]).await.unwrap();
    assert_eq!(*writer_a.next_seq.lock().unwrap(), 2);
    // A metadata snapshot row (any writer path — create_meta/touch/
    // set_title/set_pinned/backfill — stamps the writer identity), so the
    // conflict error below can name the latest snapshot writer.
    writer_a
        .create_meta(&sid, Some("m"), None, None, None, None)
        .await
        .unwrap();
    drop(writer_a);

    // Writer A2: a second concurrent writer (fresh connect, as a TUI + Web
    // pair or a second process would do) appends seqs 2..4.
    let writer_a2 = SqliteSession::connect(p, &wid, &sid).await.unwrap();
    writer_a2.append(&entries[2..4]).await.unwrap();
    assert_eq!(*writer_a2.next_seq.lock().unwrap(), 4);
    drop(writer_a2);

    // Writer B holds a stale next_seq (=2, from before A2's write) and
    // appends DIFFERENT content for seqs 2..4 → must be a conflict.
    let writer_b = SqliteSession::connect(p, &wid, &sid).await.unwrap();
    *writer_b.next_seq.lock().unwrap() = 2;
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
    assert!(err.contains("concurrent write conflict"), "got: {err}");
    assert!(err.contains(&sid), "error must name the session: {err}");
    assert!(
        err.contains("max seq") && err.contains("expected to start"),
        "error must report both the DB max seq and this writer's start seq: {err}"
    );
    assert!(
        err.contains("latest metadata writer") && err.contains(process_identity()),
        "conflict message must name the latest snapshot writer: {err}"
    );

    // The rejected attempt wrote nothing (detection happens before any
    // INSERT), so the DB is untouched.
    let loaded = writer_b.load().await.unwrap();
    assert_eq!(loaded.len(), 4, "rejected attempt must not write rows");

    // Idempotent retry: writer B re-appends A2's EXACT seqs 2..4 → Ok
    // (folded as its own earlier commit), no duplicate rows.
    *writer_b.next_seq.lock().unwrap() = 2;
    writer_b.append(&entries[2..4]).await.unwrap();
    assert_eq!(
        *writer_b.next_seq.lock().unwrap(),
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
    let (_dir, session, _sid) = fresh_session().await;
    let entries = test_entries();

    session.append(&entries[..2]).await.unwrap();
    assert_eq!(*session.next_seq.lock().unwrap(), 2);
    // Retry with the cursor the caller still holds after the (errored)
    // first attempt: same seqs, same payloads.
    *session.next_seq.lock().unwrap() = 0;
    session.append(&entries[..2]).await.unwrap();
    assert_eq!(
        *session.next_seq.lock().unwrap(),
        2,
        "idempotent retry keeps the cursor"
    );

    let loaded = session.load().await.unwrap();
    assert_eq!(loaded.len(), 2, "idempotent retry must fold, not duplicate");
    for (got, want) in loaded.iter().zip(entries[..2].iter()) {
        assert_eq!(got, want, "content mismatch after idempotent retry");
    }
}

#[tokio::test]
async fn append_partial_overlap_resumes_remainder() {
    let (_dir, path) = temp_db();
    let wid = workspace_id();
    let sid = format!("test-sql-partial-{}", crate::session::new_id());
    let p = path.to_str().unwrap();
    let entries = test_entries();

    // Writer A: seqs 0..2.
    let writer_a = SqliteSession::connect(p, &wid, &sid).await.unwrap();
    writer_a.append(&entries[..2]).await.unwrap();
    drop(writer_a);

    // The "first chunk" of writer B's slice: seqs 2..4 committed by
    // another connection (same content B will retry).
    let writer_b = SqliteSession::connect(p, &wid, &sid).await.unwrap();
    *writer_b.next_seq.lock().unwrap() = 2;
    writer_b.append(&entries[2..4]).await.unwrap();
    assert_eq!(*writer_b.next_seq.lock().unwrap(), 4);
    // Simulate the error: B still holds the pre-append cursor.
    *writer_b.next_seq.lock().unwrap() = 2;

    // Retry the full 4-entry slice: overlap [2,4) matches, remainder
    // [4,6) is inserted at seqs 4,5.
    writer_b.append(&entries[2..6]).await.unwrap();
    assert_eq!(
        *writer_b.next_seq.lock().unwrap(),
        6,
        "cursor advances past the remainder"
    );

    let loaded = writer_b.load().await.unwrap();
    assert_eq!(loaded.len(), 6, "prefix folded + remainder inserted");
    for (got, want) in loaded.iter().zip(entries[..6].iter()) {
        assert_eq!(got, want, "content mismatch after partial-overlap retry");
    }
}

#[tokio::test]
async fn toctou_sync_next_seq_from_load() {
    let (_dir, path) = temp_db();
    let wid = workspace_id();
    let sid = format!("test-sql-toctou-{}", crate::session::new_id());
    let p = path.to_str().unwrap();
    let entries = test_entries();

    // Writer A: write seq 0..2.
    let writer_a = SqliteSession::connect(p, &wid, &sid).await.unwrap();
    writer_a.append(&entries[..3]).await.unwrap();
    drop(writer_a);

    // Writer B: connect (reads max_seq=2, sets next_seq=3).
    let writer_b = SqliteSession::connect(p, &wid, &sid).await.unwrap();

    // Simulate concurrent writer A' appending seq 3..4 between connect and load.
    let writer_a2 = SqliteSession::connect(p, &wid, &sid).await.unwrap();
    writer_a2.append(&entries[3..5]).await.unwrap();
    drop(writer_a2);

    // Now writer B loads — gets 5 entries (0..4) even though its next_seq
    // is still 3 (stale from connect). This is the TOCTOU re-read.
    let loaded = writer_b.load_with_seq().await.unwrap();
    assert_eq!(loaded.len(), 5, "writer B sees all 5 entries");

    // Sync next_seq from the loaded snapshot (what the importer does
    // after TOCTOU check). Next seq should be 5.
    writer_b
        .advance_next_seq_from_snapshot_len(loaded.len())
        .unwrap();
    assert_eq!(*writer_b.next_seq.lock().unwrap(), 5);

    // Now append 2 more entries; they should get seq 5..6, not overlap.
    writer_b.append(&entries[5..7]).await.unwrap();
    let all = writer_b.load().await.unwrap();
    assert_eq!(all.len(), 7, "all 7 entries present, no overlap");
    for (got, want) in all.iter().zip(entries[..7].iter()) {
        assert_eq!(got, want, "content mismatch after TOCTOU-safe append");
    }
}

// ----------------------------------------------------------------------
// compaction-segment paging
// ----------------------------------------------------------------------

#[tokio::test]
async fn load_head_and_load_older_segmented() {
    let (_dir, session, _sid) = fresh_session().await;

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
        "head must contain comp2 + latest"
    );
    assert_eq!(head[0], comp2, "head opens with the last compaction");
    for (got, want) in head.iter().skip(1).zip(latest.iter()) {
        assert_eq!(got, want, "head tail mismatch");
    }

    // Middle segment: load_older(comp2_seq, None) = [comp1, comp2) =
    // comp1 + middle, cursor = comp1_seq.
    let (seg, cursor) = session.load_older(comp2_seq, None).await.unwrap();
    assert_eq!(cursor, Some(comp1_seq), "middle segment cursor");
    assert_eq!(seg.len(), 1 + middle.len(), "middle segment contents");
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

    // Exactly-once coverage across the chain.
    let total: usize = head.len() + seg.len() + oldest.len();
    assert_eq!(total, all.len(), "segments must cover the whole session");
}

/// Backend side of the `GET /history` initial-render fix: the head
/// segment is `[last_comp, ∞)`, and `SessionStore::load_head_page`
/// loads it as `load_older(HEAD_OPEN_SENTINEL /* i64::MAX */, limit)`
/// (see `session_store.rs`). The bounded head page must never strand the
/// truncated part of the head segment — the returned cursor feeds
/// straight back into `load_older` and the whole chain covers every
/// entry exactly once.
#[tokio::test]
async fn load_older_head_open_sentinel_pages_without_losing_segments() {
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

    // ---- No compaction: the whole session is one head segment. With no
    // limit the whole session comes back with a None cursor (nothing
    // older to page)...
    let (_dir, session, _sid) = fresh_session().await;
    let plain = vec![user(1), user(2), user(3)];
    session.append(&plain).await.unwrap();
    let (page, cursor) = session.load_older(i64::MAX, None).await.unwrap();
    assert_eq!(page, plain, "no-compaction session returned whole");
    assert_eq!(cursor, None, "no compaction → no older cursor");
    // ...and a bounded page still pages through the rest of the session
    // (nothing is stranded).
    let (page, cursor) = session.load_older(i64::MAX, Some(2)).await.unwrap();
    assert_eq!(page, vec![user(2), user(3)], "newest limit entries");
    assert_eq!(cursor, Some(1), "cursor = oldest seq of the page");
    let (rest, cursor) = session.load_older(cursor.unwrap(), Some(2)).await.unwrap();
    assert_eq!(rest, vec![user(1)], "remaining entry reachable");
    assert_eq!(cursor, None, "seq 0 page → nothing older");

    // ---- Compaction with head ≤ limit: whole head segment + cursor =
    // the opening compaction's seq.
    // seqs: 0,1 early; 2 comp1; 3,4 middle; 5 comp2; 6,7 latest.
    let (_dir, session_b, _sid) = fresh_session().await;
    let mut all = vec![user(1), user(2)];
    all.push(comp("c1"));
    all.extend([user(3), user(4)]);
    all.push(comp("c2"));
    all.extend([user(5), user(6)]);
    session_b.append(&all).await.unwrap();
    let (page, cursor) = session_b.load_older(i64::MAX, Some(3)).await.unwrap();
    assert_eq!(
        page,
        vec![comp("c2"), user(5), user(6)],
        "head ≤ limit → whole head segment"
    );
    assert_eq!(cursor, Some(5), "cursor = opening compaction seq");

    // ---- Compaction with head > limit: newest `limit` entries, cursor =
    // oldest seq of the page; paging back with that cursor reaches the
    // cut-off part, then crosses compaction boundaries — every entry is
    // covered exactly once.
    // seqs: 0,1 early; 2 comp1; 3,4 middle; 5 comp2; 6..9 latest.
    let (_dir, session_c, _sid) = fresh_session().await;
    let mut all = vec![user(1), user(2)];
    all.push(comp("c1"));
    all.extend([user(3), user(4)]);
    all.push(comp("c2"));
    all.extend([user(5), user(6), user(7), user(8)]);
    session_c.append(&all).await.unwrap();

    let mut paged: Vec<SessionEntry> = Vec::new();
    let mut cursor: Option<i64> = Some(i64::MAX); // head open sentinel
    loop {
        let (entries, next) = match cursor {
            Some(i64::MAX) => session_c.load_older(i64::MAX, Some(2)).await.unwrap(),
            Some(before) => session_c.load_older(before, Some(2)).await.unwrap(),
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
    let (page, cursor) = session_c.load_older(i64::MAX, Some(2)).await.unwrap();
    assert_eq!(page, vec![user(7), user(8)], "newest 2 of head");
    let (next, _) = session_c
        .load_older(cursor.unwrap(), Some(2))
        .await
        .unwrap();
    assert_eq!(
        next,
        vec![user(5), user(6)],
        "cut-off head part reachable via cursor"
    );
}

/// Major B: limited `load_older` pages must be sized on DISTINCT seqs,
/// not physical rows. Same-seq duplicate rows (retried writes with newer
/// event_times) previously consumed `LIMIT` slots before the Rust-side
/// dedup, so pages could come back smaller than `limit` and the cursor
/// chain could skip or repeat entries. With the SQL-side dedup
/// (`MAX(event_time_us)` per seq) before `LIMIT`, each page holds exactly
/// `limit` distinct entries and the chain pages through every seq exactly
/// once.
#[tokio::test]
async fn load_older_limited_pages_dedup_before_limit() {
    let (_dir, session, _sid) = fresh_session().await;

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

    // seqs: 0,1 early; 2 comp1; 3..7 latest (comp1_seq = 2).
    let mut all = vec![user(1), user(2)];
    all.push(comp("c1"));
    all.extend([user(3), user(4), user(5), user(6), user(7)]);
    session.append(&all).await.unwrap();

    // Retried writes: extra physical rows for seqs 6 and 7 with strictly
    // newer event_times (identical payloads — exactly what a
    // committed-then-retried append produces). 8 distinct seqs, 10
    // physical rows.
    let conn = session.conn.lock().await;
    for seq in [6i64, 7i64] {
        let entry = user(seq as u32);
        conn.execute(
            "INSERT INTO session_entries \
             (workspace_id, session_id, seq, event_time_us, entry_kind, payload, \
              schema_version, is_error) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, 0)",
            (
                session.workspace_id.as_str(),
                session.session_id.as_str(),
                seq,
                next_event_time_us(),
                entry_kind(&entry),
                serde_json::to_string(&entry).unwrap(),
            ),
        )
        .await
        .unwrap();
    }
    drop(conn);

    // Page 1 (head open sentinel, limit 2): newest 2 DISTINCT seqs = 7,6
    // — one entry each, the duplicate physical rows must NOT consume
    // limit slots.
    let (page1, cursor1) = session.load_older(i64::MAX, Some(2)).await.unwrap();
    assert_eq!(page1, vec![user(6), user(7)], "page 1 = newest 2 distinct");
    assert_eq!(cursor1, Some(6), "cursor = oldest seq of page 1");

    // Page 2: [2,6) — seqs 5,4.
    let (page2, cursor2) = session.load_older(cursor1.unwrap(), Some(2)).await.unwrap();
    assert_eq!(page2, vec![user(4), user(5)], "page 2 = next 2 distinct");
    assert_eq!(cursor2, Some(4));

    // Page 3: [2,4) — seqs 3,2; the page reaches the segment's opening
    // compaction row (seq 2 = prev_comp), cursor stays Some(2).
    let (page3, cursor3) = session.load_older(cursor2.unwrap(), Some(2)).await.unwrap();
    assert_eq!(page3, vec![comp("c1"), user(3)], "page 3 crosses to comp");
    assert_eq!(cursor3, Some(2));

    // Page 4 (oldest segment [0,2)): seqs 1,0, cursor None.
    let (page4, cursor4) = session.load_older(cursor3.unwrap(), Some(2)).await.unwrap();
    assert_eq!(page4, vec![user(1), user(2)], "page 4 = oldest segment");
    assert_eq!(cursor4, None, "cursor None at the true start");

    // Exactly-once coverage across the paged chain (multiset style).
    let mut paged: Vec<SessionEntry> = Vec::new();
    paged.extend(page1);
    paged.extend(page2);
    paged.extend(page3);
    paged.extend(page4);
    assert_eq!(
        paged.len(),
        all.len(),
        "paged chain must not lose or duplicate entries"
    );
    for want in &all {
        assert!(paged.contains(want), "paged chain missing entry: {want:?}");
    }

    // Same guarantee without a compaction (whole session is one head
    // segment): dup rows on seqs 4,5; limit 2 pages of the oldest branch.
    let (_dir, session_b, _sid) = fresh_session().await;
    let plain: Vec<SessionEntry> = (0..6).map(user).collect();
    session_b.append(&plain).await.unwrap();
    let conn = session_b.conn.lock().await;
    for seq in [4i64, 5i64] {
        let entry = user(seq as u32);
        conn.execute(
            "INSERT INTO session_entries \
             (workspace_id, session_id, seq, event_time_us, entry_kind, payload, \
              schema_version, is_error) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, 0)",
            (
                session_b.workspace_id.as_str(),
                session_b.session_id.as_str(),
                seq,
                next_event_time_us(),
                entry_kind(&entry),
                serde_json::to_string(&entry).unwrap(),
            ),
        )
        .await
        .unwrap();
    }
    drop(conn);

    let (page, cursor) = session_b.load_older(i64::MAX, Some(2)).await.unwrap();
    assert_eq!(
        page,
        vec![user(4), user(5)],
        "oldest branch: newest 2 distinct"
    );
    assert_eq!(cursor, Some(4));
    let (page, cursor) = session_b
        .load_older(cursor.unwrap(), Some(2))
        .await
        .unwrap();
    assert_eq!(page, vec![user(2), user(3)]);
    assert_eq!(cursor, Some(2));
    let (page, cursor) = session_b
        .load_older(cursor.unwrap(), Some(2))
        .await
        .unwrap();
    assert_eq!(page, vec![user(0), user(1)]);
    assert_eq!(cursor, None);
}

#[tokio::test]
async fn load_older_pages_within_segment() {
    let (_dir, session, _sid) = fresh_session().await;

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
    // 7 middle entries: enough that one segment needs several 2-entry pages.
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

    // Page 1 of the middle segment [2,10): the 2 entries closest to comp2
    // (tail) = seqs 9,8 → ascending [middle 5, middle 6]; cursor = 8.
    let (page1, cursor1) = session.load_older(comp2_seq, Some(2)).await.unwrap();
    assert_eq!(page1, vec![middle[5].clone(), middle[6].clone()]);
    assert_eq!(cursor1, Some(8));

    // Page 2: [2,8) tail = seqs 7,6 → [middle 3, middle 4]; cursor 6.
    let (page2, cursor2) = session.load_older(cursor1.unwrap(), Some(2)).await.unwrap();
    assert_eq!(page2, vec![middle[3].clone(), middle[4].clone()]);
    assert_eq!(cursor2, Some(6));

    // Page 3: [2,6) tail = seqs 5,4 → [middle 1, middle 2]; cursor 4.
    let (page3, cursor3) = session.load_older(cursor2.unwrap(), Some(2)).await.unwrap();
    assert_eq!(page3, vec![middle[1].clone(), middle[2].clone()]);
    assert_eq!(cursor3, Some(4));

    // Page 4: [2,4) = seqs 3,2 → [comp1, middle 0]; the page reaches the
    // segment's opening compaction row (seq 2 = prev_comp), so the cursor
    // stays Some(2) and the next call crosses into the older segment.
    let (page4, cursor4) = session.load_older(cursor3.unwrap(), Some(2)).await.unwrap();
    assert_eq!(page4, vec![comp1.clone(), middle[0].clone()]);
    assert_eq!(cursor4, Some(comp1_seq));

    // Page 5 (oldest segment [0,2) = early): 2 entries ≤ limit → whole
    // segment, cursor None — nothing older exists.
    let (page5, cursor5) = session.load_older(cursor4.unwrap(), Some(2)).await.unwrap();
    assert_eq!(page5, early);
    assert_eq!(cursor5, None, "cursor must be None at the true start");

    // Exactly-once coverage across the paged chain.
    let head = session.load_head().await.unwrap();
    let total: usize =
        head.len() + page1.len() + page2.len() + page3.len() + page4.len() + page5.len();
    assert_eq!(total, all.len(), "paged chain must cover the whole session");
}

#[tokio::test]
async fn load_oldest_and_load_newer_segmented() {
    let (_dir, session, _sid) = fresh_session().await;

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

    // Oldest segment: load_oldest() = [0, comp1) = early, cursor = comp1.
    let (oldest, cursor) = session.load_oldest().await.unwrap();
    assert_eq!(
        cursor,
        Some(comp1_seq),
        "oldest cursor = first compaction seq"
    );
    assert_eq!(oldest, early, "oldest segment contains only early entries");

    // Middle segment: load_newer(comp1) = [comp1, comp2) = comp1 + middle,
    // cursor = comp2.
    let (middle_seg, cursor) = session.load_newer(comp1_seq).await.unwrap();
    assert_eq!(cursor, Some(comp2_seq), "middle segment cursor");
    assert_eq!(middle_seg.len(), 1 + middle.len());
    assert_eq!(middle_seg[0], comp1, "middle segment opens with comp1");
    for (got, want) in middle_seg.iter().skip(1).zip(middle.iter()) {
        assert_eq!(got, want, "middle segment tail mismatch");
    }

    // Head boundary: load_newer(comp2) must return nothing — the head
    // segment [comp2, ∞) is already loaded by load_head.
    let (after, cursor) = session.load_newer(comp2_seq).await.unwrap();
    assert_eq!(cursor, None, "no compaction after comp2 → cursor None");
    assert!(
        after.is_empty(),
        "load_newer must never return the head segment"
    );

    // Exactly-once coverage: oldest + middle + head.
    let head = session.load_head().await.unwrap();
    let total: usize = oldest.len() + middle_seg.len() + head.len();
    assert_eq!(total, all.len(), "segments must cover the whole session");

    // A session with no compaction at all: load_oldest reports nothing
    // older (the head segment already covers everything).
    let (_dir2, no_comp, _sid2) = fresh_session().await;
    no_comp.append(&early).await.unwrap();
    let (entries, cursor) = no_comp.load_oldest().await.unwrap();
    assert!(entries.is_empty(), "no compaction → nothing older to load");
    assert_eq!(cursor, None, "no compaction → cursor None");
    let (entries, cursor) = no_comp.load_newer(0).await.unwrap();
    assert!(entries.is_empty());
    assert_eq!(cursor, None);
}

#[tokio::test]
async fn load_head_and_older_on_empty_and_tiny_sessions() {
    // Empty session: head is empty, older paging reports nothing.
    let (_dir, session, _sid) = fresh_session().await;
    assert!(session.load_head().await.unwrap().is_empty());
    assert_eq!(session.head_seq().await.unwrap(), None);
    let (entries, cursor) = session.load_older(0, None).await.unwrap();
    assert!(entries.is_empty());
    assert_eq!(cursor, None);
    let (entries, cursor) = session.load_older(0, Some(5)).await.unwrap();
    assert!(entries.is_empty());
    assert_eq!(cursor, None);

    // Tiny session with no compaction: head covers everything; load_older
    // with before=1 returns the whole oldest segment [0, 1) — the single
    // entry — and a None cursor (the page reached seq 0).
    let entry: SessionEntry = Message::User {
        content: "tiny".into(),
        images: vec![],
    }
    .into();
    session.append(std::slice::from_ref(&entry)).await.unwrap();
    let head = session.load_head().await.unwrap();
    assert_eq!(head, vec![entry.clone()]);
    let (entries, cursor) = session.load_older(1, None).await.unwrap();
    assert_eq!(entries, vec![entry.clone()]);
    assert_eq!(cursor, None);

    // A page limit larger than the remaining segment still returns the
    // whole segment and a None cursor.
    let (entries, cursor) = session.load_older(1, Some(10)).await.unwrap();
    assert_eq!(entries, vec![entry]);
    assert_eq!(cursor, None);
}

// ----------------------------------------------------------------------
// dedup_raw_entries — pure unit tests (no live DB)
// ----------------------------------------------------------------------

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

#[test]
fn dedup_older_overwritten_different_payload() {
    let raw = vec![
        (5i64, et(2000), msg_entry("new")),
        (5i64, et(1000), msg_entry("old")),
    ];
    let result =
        dedup_raw_entries(&raw, "test-session", "test-workspace", "event_time_us").unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].0, 5);
    assert_eq!(
        serde_json::to_string(&result[0].1).unwrap(),
        msg_entry("new")
    );
}

#[test]
fn dedup_older_overwritten_three_versions() {
    let raw = vec![
        (1i64, et(3000), msg_entry("v3")),
        (1i64, et(1000), msg_entry("v1")),
        (1i64, et(2000), msg_entry("v2")),
    ];
    let result =
        dedup_raw_entries(&raw, "test-session", "test-workspace", "event_time_us").unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(
        serde_json::to_string(&result[0].1).unwrap(),
        msg_entry("v3")
    );
}

#[test]
fn dedup_older_overwritten_identical_payload() {
    let raw = vec![
        (5i64, et(2000), msg_entry("hello")),
        (5i64, et(1000), msg_entry("hello")),
    ];
    let result =
        dedup_raw_entries(&raw, "test-session", "test-workspace", "event_time_us").unwrap();
    assert_eq!(result.len(), 1);
}

#[test]
fn dedup_latest_tie_identical_folded() {
    let raw = vec![
        (3i64, et(500), msg_entry("hello")),
        (3i64, et(500), msg_entry("hello")),
    ];
    let result =
        dedup_raw_entries(&raw, "test-session", "test-workspace", "event_time_us").unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].0, 3);
}

#[test]
fn dedup_latest_tie_three_identical_folded() {
    let raw = vec![
        (7i64, et(999), msg_entry("triple")),
        (7i64, et(999), msg_entry("triple")),
        (7i64, et(999), msg_entry("triple")),
    ];
    let result =
        dedup_raw_entries(&raw, "test-session", "test-workspace", "event_time_us").unwrap();
    assert_eq!(result.len(), 1);
}

#[test]
fn dedup_latest_tie_divergent_rejected() {
    let raw = vec![
        (3i64, et(500), msg_entry("hello")),
        (3i64, et(500), msg_entry("world")),
    ];
    let err =
        dedup_raw_entries(&raw, "test-session", "test-workspace", "event_time_us").unwrap_err();
    assert!(err.contains("divergent physical duplicates"), "got: {err}");
    assert!(err.contains("seq 3"), "got: {err}");
}

#[test]
fn dedup_latest_tie_divergent_with_older_rows() {
    let raw = vec![
        (5i64, et(2000), msg_entry("new_a")),
        (5i64, et(2000), msg_entry("new_b")), // divergent!
        (5i64, et(1000), msg_entry("old")),
    ];
    let err =
        dedup_raw_entries(&raw, "test-session", "test-workspace", "event_time_us").unwrap_err();
    assert!(err.contains("divergent physical duplicates"), "got: {err}");
    assert!(err.contains("seq 5"), "got: {err}");
}

#[test]
fn dedup_output_winning_event_time_precedes_seq() {
    let raw = vec![
        (10i64, et(3000), msg_entry("ten")),
        (5i64, et(1000), msg_entry("five")),
        (1i64, et(2000), msg_entry("one")),
    ];
    let result =
        dedup_raw_entries(&raw, "test-session", "test-workspace", "event_time_us").unwrap();
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].0, 5, "earliest winning event_time first");
    assert_eq!(result[1].0, 1, "middle winning event_time second");
    assert_eq!(result[2].0, 10, "latest winning event_time last");
}

#[test]
fn dedup_output_seq_tiebreaks_same_event_time() {
    let raw = vec![
        (20i64, et(1000), msg_entry("seq20")),
        (5i64, et(1000), msg_entry("seq5")),
    ];
    let result =
        dedup_raw_entries(&raw, "test-session", "test-workspace", "event_time_us").unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].0, 5, "lower seq first");
    assert_eq!(result[1].0, 20, "higher seq second");
}

#[test]
fn dedup_versions_use_winning_time_for_canonical_order() {
    let raw = vec![
        (0i64, et(100), msg_entry("seq0-old")),
        (0i64, et(400), msg_entry("seq0-new")),
        (0i64, et(400), msg_entry("seq0-new")), // winning tie folds
        (1i64, et(300), msg_entry("seq1")),
    ];
    let result =
        dedup_raw_entries(&raw, "test-session", "test-workspace", "event_time_us").unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].0, 1);
    assert_eq!(result[1].0, 0);
    assert_eq!(
        serde_json::to_string(&result[1].1).unwrap(),
        msg_entry("seq0-new")
    );
}

#[test]
fn dedup_empty_input() {
    let result = dedup_raw_entries(&[], "test-session", "test-workspace", "event_time_us").unwrap();
    assert!(result.is_empty());
}

#[test]
fn dedup_multi_seq_mixed_older_latest() {
    let raw = vec![
        (1i64, et(100), msg_entry("seq1-old")),
        (1i64, et(300), msg_entry("seq1-latest")),
        (2i64, et(200), msg_entry("seq2-only")),
        (3i64, et(150), msg_entry("seq3-only")),
        (3i64, et(150), msg_entry("seq3-only")), // folded: same et, same payload
    ];
    let result =
        dedup_raw_entries(&raw, "test-session", "test-workspace", "event_time_us").unwrap();
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].0, 3);
    assert_eq!(result[1].0, 2);
    assert_eq!(result[2].0, 1);
    assert_eq!(
        serde_json::to_string(&result[2].1).unwrap(),
        msg_entry("seq1-latest")
    );
}

// ----------------------------------------------------------------------
// pure helpers
// ----------------------------------------------------------------------

#[test]
fn workspace_id_is_stable() {
    let a = derive_workspace_id(Path::new("/tmp/ws"));
    let b = derive_workspace_id(Path::new("/tmp/ws"));
    assert_eq!(a, b);
}

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
        "pid must be ours: {first}"
    );
    let (hostname, nonce) = rest.split_once('#').expect("has # separator");
    assert!(
        !hostname.is_empty(),
        "hostname fallback must never be empty: {first}"
    );
    assert!(!nonce.is_empty(), "nonce must never be empty: {first}");
}

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
    let backfilled = SqliteSession::backfill_meta_snapshot(
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
        "preserve original creation time"
    );
    assert_eq!(backfilled.last_active_at, now, "fresh last_active_at");
    assert_eq!(
        backfilled.model.as_deref(),
        Some("model-y"),
        "caller model wins"
    );
    assert_eq!(
        backfilled.role.as_deref(),
        Some("main"),
        "existing role kept"
    );
    assert_eq!(backfilled.entry_count, 3);
    assert_eq!(backfilled.parent_session_id.as_deref(), Some("parent-1"));
    assert_eq!(backfilled.parent_task_id, Some(7));
    assert_eq!(backfilled.title.as_deref(), Some("a title"));
    assert_eq!(backfilled.pinned, Some(true));
    assert_eq!(backfilled.archived, Some(true));
    assert_eq!(backfilled.writer, None, "writer is stamped by insert_meta");

    // Model-only backfill leaves the (absent) links untouched.
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
    let model_only = SqliteSession::backfill_meta_snapshot(
        &no_parent,
        us_to_datetime(next_event_time_us()),
        Some("model-z"),
        None,
        None,
        None,
    );
    assert_eq!(model_only.model.as_deref(), Some("model-z"));
    assert_eq!(model_only.parent_session_id, None, "no parent injected");
    assert_eq!(model_only.parent_task_id, None);
}

#[tokio::test]
async fn advance_next_seq_rewind_rejected_and_toctou_refresh() {
    let (_dir, session, _sid) = fresh_session().await;
    // After connect, next_seq is 0 (empty session → max_seq=-1 → +1 = 0).

    session.advance_next_seq_from_snapshot_len(5).unwrap();
    let err = session.advance_next_seq_from_snapshot_len(3).unwrap_err();
    assert!(err.contains("rewind"), "got: {err}");

    // Same value is allowed (no-op advance); normal advance works.
    session.advance_next_seq_from_snapshot_len(5).unwrap();
    session.advance_next_seq_from_snapshot_len(10).unwrap();
    assert_eq!(*session.next_seq.lock().unwrap(), 10);
}

// ----------------------------------------------------------------------
// usage_entries — token-usage statistics table
// ----------------------------------------------------------------------

#[tokio::test]
async fn usage_entries_append_and_summarize() {
    let (_dir, session, sid) = fresh_session().await;
    let wid = workspace_id();

    // 空表 → 空汇总。
    assert!(session.usage_summary().await.unwrap().is_empty());

    // 同一 (session, model, kind) 多行聚合；不同 model/kind 维度分行。
    session
        .append_usage(&wid, &sid, "model-a", "regular", 100, 50)
        .await
        .unwrap();
    session
        .append_usage(&wid, &sid, "model-a", "regular", 200, 30)
        .await
        .unwrap();
    session
        .append_usage(&wid, &sid, "model-a", "compact", 1000, 200)
        .await
        .unwrap();
    session
        .append_usage(&wid, &sid, "model-b", "regular", 10, 5)
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
    assert_eq!(compact_a.output_tokens, 200);
    let regular_b = rows
        .iter()
        .find(|r| r.model == "model-b" && r.kind == "regular")
        .expect("regular/model-b group");
    assert_eq!(regular_b.input_tokens, 10);
    assert_eq!(regular_b.output_tokens, 5);
}

#[tokio::test]
async fn usage_entries_are_scoped_by_workspace() {
    let (_dir, session, sid) = fresh_session().await;
    // 写入别的 workspace_id：本 workspace 的汇总查不到。
    session
        .append_usage("other-workspace", &sid, "m", "regular", 1, 1)
        .await
        .unwrap();
    assert!(session.usage_summary().await.unwrap().is_empty());
    // 同 workspace 的其他 session：汇总可见（usage_summary 按 workspace
    // 聚合，每行携带各自的 session_id）。
    session
        .append_usage(&workspace_id(), "other-session", "m", "regular", 2, 2)
        .await
        .unwrap();
    let rows = session.usage_summary().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].session_id, "other-session");
    assert_eq!(rows[0].input_tokens, 2);
}

#[tokio::test]
async fn usage_for_sessions_filters_and_aggregates() {
    let (_dir, session, sid) = fresh_session().await;
    let wid = workspace_id();
    let child = format!("sub-{sid}");
    let other = format!("test-sql-other-{}", crate::session::new_id());

    // 空 id 列表：短路为空（不发查询）。
    assert!(session.usage_for_sessions(&[]).await.unwrap().is_empty());

    // 本会话两行（同 model/kind 聚合）+ 子会话一行 + 无关会话一行。
    session
        .append_usage(&wid, &sid, "m", "regular", 100, 50)
        .await
        .unwrap();
    session
        .append_usage(&wid, &sid, "m", "regular", 20, 5)
        .await
        .unwrap();
    session
        .append_usage(&wid, &child, "m", "regular", 7, 3)
        .await
        .unwrap();
    session
        .append_usage(&wid, &other, "m", "regular", 999, 999)
        .await
        .unwrap();

    // 只查本会话：聚合本行，排除子会话与无关会话。
    let rows = session
        .usage_for_sessions(std::slice::from_ref(&sid))
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].session_id, sid);
    assert_eq!(rows[0].input_tokens, 120);
    assert_eq!(rows[0].output_tokens, 55);

    // 本会话 + 子会话：两行都在（服务端再把它们合计进 usage 响应）。
    let rows = session
        .usage_for_sessions(&[sid.clone(), child.clone()])
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    let input: u64 = rows.iter().map(|r| r.input_tokens).sum();
    let output: u64 = rows.iter().map(|r| r.output_tokens).sum();
    assert_eq!(input, 127);
    assert_eq!(output, 58);

    // 无关会话 id 过滤生效；未知会话 → 空。
    assert!(
        session
            .usage_for_sessions(&[other])
            .await
            .unwrap()
            .iter()
            .all(|r| r.session_id != sid)
    );
    assert!(
        session
            .usage_for_sessions(&["no-such-session".to_owned()])
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn usage_entries_persist_across_reconnect() {
    let (_dir, path) = temp_db();
    let wid = workspace_id();
    let sid = format!("test-sql-{}", crate::session::new_id());
    {
        let session = SqliteSession::connect(path.to_str().unwrap(), &wid, &sid)
            .await
            .expect("connect");
        session
            .append_usage(&wid, &sid, "m", "regular", 7, 3)
            .await
            .unwrap();
    }
    let session = SqliteSession::connect(path.to_str().unwrap(), &wid, &sid)
        .await
        .expect("reconnect");
    let rows = session.usage_summary().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].model, "m");
    assert_eq!(rows[0].kind, "regular");
    assert_eq!(rows[0].input_tokens, 7);
    assert_eq!(rows[0].output_tokens, 3);
}

#[tokio::test]
async fn store_facade_usage_append_summary_and_jsonl_noop() {
    use crate::session_store::SessionStore;

    // SQLite facade：append_usage → usage_summary 走通。
    let (_dir, path) = temp_db();
    let root = std::path::Path::new("/tmp/e-agent-test-sqlite");
    let sid = format!("test-sql-store-usage-{}", crate::session::new_id());
    let store = SessionStore::connect(
        &crate::config::SessionBackend::Sqlite {
            path: Some(path.to_string_lossy().into_owned()),
        },
        root,
        &sid,
    )
    .await
    .expect("connect sqlite store");
    store
        .append_usage(root, &sid, "m", "regular", 5, 6)
        .await
        .expect("append usage via facade");
    let rows = store
        .usage_summary(root)
        .await
        .expect("usage summary via facade");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].session_id, sid);
    assert_eq!(rows[0].input_tokens, 5);
    assert_eq!(rows[0].output_tokens, 6);

    // JSONL facade：静默跳过（不报错、无记录）。
    let jsonl = SessionStore::Jsonl;
    jsonl
        .append_usage(root, &sid, "m", "regular", 1, 1)
        .await
        .expect("jsonl append_usage is a silent no-op");
    assert!(
        jsonl
            .usage_summary(root)
            .await
            .expect("jsonl summary")
            .is_empty()
    );
}

// ----------------------------------------------------------------------
// running_tasks — background-task state table
// ----------------------------------------------------------------------

#[tokio::test]
async fn running_tasks_lifecycle() {
    let (_dir, session, sid) = fresh_session().await;

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
    let other = format!("test-sql-rt-other-{}", crate::session::new_id());
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
async fn running_tasks_rerecord_same_key_overwrites() {
    let (_dir, session, sid) = fresh_session().await;

    session
        .record_task_start(&sid, 42, "first label", None, None)
        .await
        .unwrap();
    // Re-record the SAME (workspace, session, task_id) without deleting:
    // Greptime's restart-time last-write-wins contract — the new write
    // overwrites the existing row instead of erroring on the primary-key
    // conflict.
    session
        .record_task_start(&sid, 42, "second label", None, Some("sub-overwrite"))
        .await
        .unwrap();

    // Raw count proves there is exactly one physical row per task key
    // (the overwrite updated in place instead of inserting a second row).
    let conn = session.conn.lock().await;
    let mut rows = conn
        .query(
            "SELECT COUNT(*) FROM running_tasks \
             WHERE workspace_id = ?1 AND session_id = ?2 AND task_id = ?3",
            (session.workspace_id.as_str(), sid.as_str(), 42i64),
        )
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    assert_eq!(
        row.get_value(0).unwrap().as_integer().copied(),
        Some(1),
        "exactly one physical row per task key after overwrite"
    );
    drop(conn);

    // The subsequent read returns the NEWEST label and subagent link.
    let labels = session.take_unfinished_tasks(&sid).await.unwrap();
    assert_eq!(labels.len(), 1, "overwrite must not duplicate the row");
    assert_eq!(
        labels[0], "task 42: second label (session: sub-overwrite)",
        "newest write must win"
    );
}

/// `record_task_start` 持久化完整命令（`full_command` 列，bash 任务传
/// `Some`、delegate 传 `None`）；`task_full_command` 按 id 读回。消费路径
/// （`take_unfinished_tasks`）仍只回 label（「被杀」Notice 文本不变），
/// 完整命令作为独立读取路径供 UI/`/api/tasks` 回退取用。
#[tokio::test]
async fn running_tasks_persists_full_command() {
    let (_dir, session, sid) = fresh_session().await;
    let long = "cargo build --release --features very-long-feature-name \
                --target x86_64-unknown-linux-gnu --jobs 8";

    // Bash-style record: full command persisted.
    session
        .record_task_start(&sid, 1, "cargo build …", Some(long), None)
        .await
        .unwrap();
    // Delegate-style record: no command → NULL column.
    session
        .record_task_start(&sid, 2, "delegate work", None, Some("sub-probe"))
        .await
        .unwrap();

    assert_eq!(
        session.task_full_command(&sid, 1).await.unwrap().as_deref(),
        Some(long),
        "full command survives the write"
    );
    assert_eq!(
        session.task_full_command(&sid, 2).await.unwrap(),
        None,
        "delegate row has no full command"
    );
    assert_eq!(
        session.task_full_command(&sid, 99).await.unwrap(),
        None,
        "unknown task id → None"
    );

    // The consumption path is unchanged: labels only.
    assert_eq!(
        session.take_unfinished_tasks(&sid).await.unwrap(),
        vec![
            "task 1: cargo build …".to_string(),
            "task 2: delegate work (session: sub-probe)".to_string(),
        ]
    );
    // Consumed rows are gone → lookup returns None.
    assert_eq!(
        session.task_full_command(&sid, 1).await.unwrap(),
        None,
        "row consumed → no full command on record"
    );

    // Re-record overwrites the full command too (last-write-wins).
    session
        .record_task_start(&sid, 1, "new label", Some("echo new"), None)
        .await
        .unwrap();
    session
        .record_task_start(&sid, 1, "new label", None, None)
        .await
        .unwrap();
    assert_eq!(
        session.task_full_command(&sid, 1).await.unwrap(),
        None,
        "re-record without a command clears the previous full command"
    );
}

/// 老库（running_tasks 表没有 full_command 列）在 connect 时自动
/// ALTER 补列；老行读回 full_command = NULL（`task_full_command` 返回
/// None，兼容旧数据），新写入（record_task_start 带 full_command）落新列。
#[tokio::test]
async fn connect_migrates_legacy_running_tasks_table_missing_full_command() {
    let (dir, path) = temp_db();
    let wid = workspace_id();
    let p = path.to_str().unwrap();

    // 手工建一个「旧版」running_tasks 表：没有 full_command 列（也没有
    // owner_identity——连迁移探测本身也走老路径），并写入一条旧行。
    let legacy_ddl = r#"
        CREATE TABLE running_tasks (
            workspace_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            task_id INTEGER NOT NULL,
            label TEXT NOT NULL,
            subagent_session_id TEXT NULL,
            started_at_us INTEGER NOT NULL,
            PRIMARY KEY (workspace_id, session_id, task_id)
        )
    "#;
    {
        let db = turso::Builder::new_local(p)
            .build()
            .await
            .expect("open legacy db");
        let conn = db.connect().expect("connect legacy db");
        conn.execute(legacy_ddl, ())
            .await
            .expect("create legacy running_tasks table");
        conn.execute(
            "INSERT INTO running_tasks \
             (workspace_id, session_id, task_id, label, started_at_us) \
             VALUES (?1, ?2, ?3, ?4, 1)",
            (wid.as_str(), "legacy-task-session", 1i64, "old build"),
        )
        .await
        .expect("insert legacy running_tasks row");
    }

    // connect 自动探测 + ALTER 补 full_command（和 owner_identity）列。
    let session = SqliteSession::connect(p, &wid, "legacy-task-session")
        .await
        .expect("connect migrates the legacy table");

    // 老行：full_command 缺失 → NULL → task_full_command 返回 None。
    assert_eq!(
        session
            .task_full_command("legacy-task-session", 1)
            .await
            .expect("read legacy row full command"),
        None,
        "pre-migration rows have no full command"
    );
    // 老行的 label 仍可正常消费（消费路径不受新列影响）。
    assert_eq!(
        session
            .take_unfinished_tasks("legacy-task-session")
            .await
            .expect("consume legacy row"),
        vec!["task 1: old build".to_string()]
    );

    // 新写入落 full_command 列：record_task_start 带命令 + 读回成功。
    session
        .record_task_start(
            "legacy-task-session",
            7,
            "new build",
            Some("cargo build"),
            None,
        )
        .await
        .expect("write full command after migration");
    assert_eq!(
        session
            .task_full_command("legacy-task-session", 7)
            .await
            .expect("read migrated full command")
            .as_deref(),
        Some("cargo build"),
        "post-migration writes persist the full command"
    );

    // 迁移幂等：再 connect 一次，探测发现列已存在、不重复 ALTER，一切正常。
    drop(session);
    let session2 = SqliteSession::connect(p, &wid, "legacy-task-session")
        .await
        .expect("reconnect on migrated db");
    assert_eq!(
        session2
            .task_full_command("legacy-task-session", 7)
            .await
            .expect("read after reconnect")
            .as_deref(),
        Some("cargo build")
    );
    drop((dir, session2));
}

/// The `owner` column records the process identity of the process that
/// started each task, and `unfinished_owner_all_dead` (the server-attach
/// probe) only reports true when EVERY surviving row was left by a
/// definitely-dead process.
#[tokio::test]
async fn running_tasks_owner_column_liveness_probe() {
    let (_dir, session, sid) = fresh_session().await;

    // No rows → all dead (vacuously: Consume would take nothing).
    assert!(
        session
            .unfinished_owner_all_dead(&sid)
            .await
            .expect("probe on empty table")
    );

    // A row recorded by THIS process (still alive) → not all dead, and
    // the owner column holds our process identity.
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
    {
        let conn = session.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT owner_identity FROM running_tasks \
                 WHERE workspace_id = ?1 AND session_id = ?2 AND task_id = ?3",
                (session.workspace_id.as_str(), sid.as_str(), 1i64),
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert_eq!(
            row.get_value(0).unwrap().as_text().cloned(),
            Some(crate::session_store::process_identity().to_owned()),
            "record must carry the recording process identity"
        );
    }

    // Rewrite the owner to a definitely-dead pid → all rows dead → true.
    // Only provable with an exported hostname: with none, the owner's
    // hostname falls back to "unknown" and stays unjudgeable → alive
    // (P2-2), which the else-branch asserts instead.
    match probeable_hostname() {
        Some(hostname) => {
            {
                let conn = session.conn.lock().await;
                conn.execute(
                    "UPDATE running_tasks SET owner_identity = ?1 \
                     WHERE workspace_id = ?2 AND session_id = ?3",
                    (
                        format!("2000000000@{hostname}#deadbeef"),
                        session.workspace_id.as_str(),
                        sid.as_str(),
                    ),
                )
                .await
                .unwrap();
            }
            assert!(
                session
                    .unfinished_owner_all_dead(&sid)
                    .await
                    .expect("probe with dead owner")
            );
        }
        None => {
            // Unjudgeable "unknown"-hostname owner: treated as alive →
            // not all dead, even though the pid itself cannot exist.
            {
                let conn = session.conn.lock().await;
                conn.execute(
                    "UPDATE running_tasks SET owner_identity = ?1 \
                     WHERE workspace_id = ?2 AND session_id = ?3",
                    (
                        "2000000000@unknown#deadbeef",
                        session.workspace_id.as_str(),
                        sid.as_str(),
                    ),
                )
                .await
                .unwrap();
            }
            assert!(
                !session
                    .unfinished_owner_all_dead(&sid)
                    .await
                    .expect("probe with unjudgeable owner")
            );
        }
    }

    // NULL owner (a row written before the column shipped) → alive.
    {
        let conn = session.conn.lock().await;
        conn.execute(
            "UPDATE running_tasks SET owner_identity = NULL \
             WHERE workspace_id = ?1 AND session_id = ?2",
            (session.workspace_id.as_str(), sid.as_str()),
        )
        .await
        .unwrap();
    }
    assert!(
        !session
            .unfinished_owner_all_dead(&sid)
            .await
            .expect("probe with NULL owner"),
        "a NULL owner (old row) must be treated as alive"
    );

    // Mixed dead + live owners → not all dead.
    session
        .record_task_start(&sid, 2, "cargo build", None, None)
        .await
        .unwrap();
    assert!(
        !session
            .unfinished_owner_all_dead(&sid)
            .await
            .expect("probe with mixed owners"),
        "one live owner makes the whole probe false"
    );

    // Consuming the rows still works unchanged (take → empty table →
    // all dead again).
    let labels = session.take_unfinished_tasks(&sid).await.unwrap();
    assert_eq!(labels.len(), 2);
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

/// The environment's exported hostname, when there is one: only then can
/// a hand-built owner identity reach the pid-probe path (P2-2 makes an
/// unset hostname unjudgeable → conservative alive).
fn probeable_hostname() -> Option<String> {
    let host = hostname_now();
    (host != "unknown").then_some(host)
}

/// 老库迁移：一个在 `owner` 列加入之前创建的 running_tasks 表（没有
/// owner 列）在 connect 时自动 ALTER 补列，随后记录写入与 liveness
/// 探测全部正常；迁移幂等（再 connect 不重复 ALTER）。
#[tokio::test]
async fn connect_migrates_legacy_running_tasks_table_missing_owner_column() {
    let (dir, path) = temp_db();
    let wid = workspace_id();
    let p = path.to_str().unwrap();
    let sid = format!("test-sql-owner-legacy-{}", crate::session::new_id());

    // 手工建一个「旧版」running_tasks 表（没有 owner 列），并写入一条
    // 旧任务行。
    let legacy_ddl = r#"
        CREATE TABLE running_tasks (
            workspace_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            task_id INTEGER NOT NULL,
            label TEXT NOT NULL,
            subagent_session_id TEXT NULL,
            started_at_us INTEGER NOT NULL,
            PRIMARY KEY (workspace_id, session_id, task_id)
        )
    "#;
    {
        let db = turso::Builder::new_local(p)
            .build()
            .await
            .expect("open legacy db");
        let conn = db.connect().expect("connect legacy db");
        conn.execute(legacy_ddl, ())
            .await
            .expect("create legacy running_tasks table");
        conn.execute(
            "INSERT INTO running_tasks \
             (workspace_id, session_id, task_id, label, started_at_us) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (
                wid.as_str(),
                sid.as_str(),
                7i64,
                "legacy task",
                1_700_000_000_000_000i64,
            ),
        )
        .await
        .expect("insert legacy running_tasks row");
    }

    // connect 应自动探测 + ALTER：老行 owner 读回 NULL（→ alive），
    // 新记录写入带 owner 列。
    let session = SqliteSession::connect(p, &wid, &sid)
        .await
        .expect("connect migrates the legacy table");
    assert!(
        !session
            .unfinished_owner_all_dead(&sid)
            .await
            .expect("probe on migrated legacy table"),
        "legacy row without owner must probe as alive"
    );
    session
        .record_task_start(&sid, 8, "new task", None, None)
        .await
        .unwrap();
    assert!(
        !session
            .unfinished_owner_all_dead(&sid)
            .await
            .expect("probe after new record on migrated table"),
        "the new row's live owner keeps the probe false"
    );

    // 迁移幂等：再 connect 一次，探测发现列已存在、不重复 ALTER。
    drop(session);
    let session2 = SqliteSession::connect(p, &wid, &sid)
        .await
        .expect("reconnect on migrated db");
    assert!(
        !session2
            .unfinished_owner_all_dead(&sid)
            .await
            .expect("probe after reconnect"),
        "reconnect must keep working"
    );
    drop((dir, session2));
}

/// P1-2 的 Err 契约：probe 查询硬失败（running_tasks 表被破坏）时返回
/// Err —— server build_session 正是靠这个 Err 降级为 Preserve（而不是
/// 让整个 session build 500）。
#[tokio::test]
async fn unfinished_owner_all_dead_probe_error_is_reported() {
    let (_dir, session, sid) = fresh_session().await;

    // 破坏 running_tasks 表，使 probe 查询报 "no such table"。
    {
        let conn = session.conn.lock().await;
        conn.execute("DROP TABLE running_tasks", ())
            .await
            .expect("drop running_tasks table");
    }
    let err = session
        .unfinished_owner_all_dead(&sid)
        .await
        .expect_err("probe must error on a broken table");
    assert!(
        err.contains("cannot load unfinished background task owners"),
        "probe error must carry context: {err}"
    );
}

/// 端到端（真实数据库文件，走 SessionStore 门面）：record（owner = 当前
/// 进程）→ all_dead = false；手改 owner 为不存在的 pid → all_dead = true；
/// Consume（take）后 → all_dead = true。这正是 server build_session 在
/// attach 时的决策路径。
#[tokio::test]
async fn store_facade_unfinished_owner_all_dead_e2e() {
    use crate::session_store::SessionStore;

    let (_dir, path) = temp_db();
    let root = std::path::Path::new("/tmp/e-agent-test-sqlite");
    let sid = format!("test-sql-store-owner-{}", crate::session::new_id());
    let store = SessionStore::connect(
        &crate::config::SessionBackend::Sqlite {
            path: Some(path.to_string_lossy().into_owned()),
        },
        root,
        &sid,
    )
    .await
    .expect("connect sqlite store");

    // 无记录：Consume 空转 → true。
    assert!(
        store
            .unfinished_owner_all_dead(root, &sid)
            .await
            .expect("probe with no records")
    );

    // record（fire-and-forget 落到当前 runtime）：owner = 当前进程（活着）
    // → 轮询直到记录落地，probe 为 false。
    store.record_background_start(root, &sid, 7, "build project", None, None);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if !store
            .unfinished_owner_all_dead(root, &sid)
            .await
            .expect("probe with live owner")
        {
            break; // 记录已落地
        }
        assert!(
            std::time::Instant::now() < deadline,
            "background record never landed"
        );
        tokio::task::yield_now().await;
    }

    // 手改 owner 为不存在的 pid（同一 hostname）→ 全部 dead → true。
    // 仅当环境导出了 hostname 时可证明；否则 owner 侧 hostname 回退为
    // "unknown" → 不可判断 → alive（P2-2），走 else 分支断言保守结果。
    match probeable_hostname() {
        Some(hostname) => {
            {
                let db = turso::Builder::new_local(path.to_str().unwrap())
                    .build()
                    .await
                    .expect("open db for hand edit");
                let conn = db.connect().expect("connect for hand edit");
                conn.execute(
                    "UPDATE running_tasks SET owner_identity = ?1 \
                     WHERE workspace_id = ?2 AND session_id = ?3",
                    (
                        format!("2000000000@{hostname}#deadbeef"),
                        workspace_id(),
                        sid.as_str(),
                    ),
                )
                .await
                .expect("rewrite owner to dead pid");
            }
            assert!(
                store
                    .unfinished_owner_all_dead(root, &sid)
                    .await
                    .expect("probe with dead owner")
            );
        }
        None => {
            let db = turso::Builder::new_local(path.to_str().unwrap())
                .build()
                .await
                .expect("open db for hand edit");
            let conn = db.connect().expect("connect for hand edit");
            conn.execute(
                "UPDATE running_tasks SET owner_identity = ?1 \
                 WHERE workspace_id = ?2 AND session_id = ?3",
                ("2000000000@unknown#deadbeef", workspace_id(), sid.as_str()),
            )
            .await
            .expect("rewrite owner to unjudgeable pid");
            assert!(
                !store
                    .unfinished_owner_all_dead(root, &sid)
                    .await
                    .expect("probe with unjudgeable owner")
            );
        }
    }

    // Consume（take）后无剩余记录 → 又变回 true。
    let labels = store
        .take_unfinished_background(root, &sid)
        .await
        .expect("take after all owners dead");
    assert_eq!(
        labels,
        vec![crate::session::format_unfinished(7, "build project", None)]
    );
    assert!(
        store
            .unfinished_owner_all_dead(root, &sid)
            .await
            .expect("probe after consume")
    );
}

#[tokio::test]
async fn running_tasks_subagent_lookup_crosses_parent_sessions() {
    let (_dir, session, subagent) = fresh_session().await;

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
    assert!(labels.contains(&"task 11: probe b".to_string()));
    // Consumed: a second lookup finds nothing.
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

#[tokio::test]
async fn running_tasks_label_for_subagent_returns_latest() {
    let (_dir, session, subagent) = fresh_session().await;
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
    let (_dir, session, _sid) = fresh_session().await;
    assert!(
        session.all_subagent_labels().await.unwrap().is_empty(),
        "no rows yet → empty map"
    );

    // Two delegate tasks for the same subagent; the newest started_at wins
    // (record_task_start timestamps are strictly increasing).
    let subagent = format!("sub-batch-{}", crate::session::new_id());
    let parent = format!("parent-batch-{}", crate::session::new_id());
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
    let other = format!("other-batch-{}", crate::session::new_id());
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

// ----------------------------------------------------------------------
// sessions — metadata audit table
// ----------------------------------------------------------------------

#[tokio::test]
async fn sessions_meta_create_list_touch_audit_delete() {
    let (_dir, session, sid) = fresh_session().await;

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

    // create is idempotent: a second create appends nothing.
    session
        .create_meta(&sid, Some("model-x"), Some("main"), None, None, None)
        .await
        .unwrap();
    assert_eq!(
        session.audit_meta(&sid).await.unwrap().len(),
        1,
        "re-create must not append a second creation snapshot"
    );

    // touch twice → audit trail has 3 rows (create + 2 touches), the list
    // still returns one latest snapshot, created_at survives.
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
        "touch preserves created_at"
    );
    assert!(
        latest.last_active_at > created.last_active_at,
        "touch advances last_active_at"
    );

    // delete hides the session entirely (all audit rows gone).
    session.delete_meta(&sid).await.unwrap();
    assert!(session.audit_meta(&sid).await.unwrap().is_empty());
    assert!(
        !session
            .list_meta()
            .await
            .unwrap()
            .iter()
            .any(|m| m.session_id == sid),
        "deleted session must not be listed"
    );

    // ...and create_meta works again afterwards (delete → create cycle).
    session
        .create_meta(&sid, Some("model-x"), Some("main"), None, None, None)
        .await
        .unwrap();
    assert_eq!(session.audit_meta(&sid).await.unwrap().len(), 1);
    assert!(
        session
            .list_meta()
            .await
            .unwrap()
            .iter()
            .any(|m| m.session_id == sid)
    );
}

#[tokio::test]
async fn sessions_meta_create_title_preserved_on_resume_backfill() {
    let (_dir, session, sid) = fresh_session().await;

    // Fresh creation with a title.
    session
        .create_meta(
            &sid,
            Some("model-x"),
            Some("main"),
            None,
            None,
            Some("original title"),
        )
        .await
        .unwrap();
    assert_eq!(session.audit_meta(&sid).await.unwrap().len(), 1);

    // Resume (row exists, nothing missing): even a different title is a
    // no-op — the creation title survives and nothing is appended.
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
        "resume must not append a snapshot or rewrite the title"
    );
    let list = session.list_meta().await.unwrap();
    assert_eq!(
        list[0].title.as_deref(),
        Some("original title"),
        "resume keeps the creation title"
    );

    // Backfill (missing model on a pre-table row) also preserves the title.
    let (_, session2, sid2) = fresh_session().await;
    session2
        .create_meta(&sid2, None, None, None, None, Some("pre-table title"))
        .await
        .unwrap();
    session2
        .create_meta(
            &sid2,
            Some("model-y"),
            None,
            Some("parent-1"),
            Some(7),
            None,
        )
        .await
        .unwrap();
    let latest = session2
        .list_meta()
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.session_id == sid2)
        .expect("session listed");
    assert_eq!(
        latest.title.as_deref(),
        Some("pre-table title"),
        "backfill carries the existing title, never overwrites it"
    );
    assert_eq!(latest.model.as_deref(), Some("model-y"));
}

#[tokio::test]
async fn sessions_meta_writer_is_process_identity() {
    // File-backed so a fresh connect can re-read the stamped writer from
    // the snapshot rows.
    let (_dir, path) = temp_db();
    let wid = workspace_id();
    let sid = format!("test-sql-meta-writer-{}", crate::session::new_id());
    let p = path.to_str().unwrap();
    let session = SqliteSession::connect(p, &wid, &sid).await.unwrap();

    session
        .create_meta(&sid, Some("model-x"), None, None, None, None)
        .await
        .unwrap();
    session.touch_meta().await.unwrap();
    session.touch_meta().await.unwrap();

    let list = session.list_meta().await.unwrap();
    let latest = list
        .iter()
        .find(|m| m.session_id == sid)
        .expect("created session must be listed");
    assert_eq!(latest.writer.as_deref(), Some(process_identity()));

    // The audit trail stamps every row, not just the newest.
    for row in session.audit_meta(&sid).await.unwrap() {
        assert_eq!(row.writer.as_deref(), Some(process_identity()));
    }
    drop(session);

    // A fresh connect re-reads the stamped writer from the DB (the cached
    // row is built from the table, not reconstructed).
    let resumed = SqliteSession::connect(p, &wid, &sid).await.unwrap();
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
    let (_dir, session, sid) = fresh_session().await;
    let entries = test_entries();

    // entry_count = next_seq (MAX(seq)+1 at connect, advanced by append) —
    // not a physical row count.
    session.append(&entries[..3]).await.unwrap();
    assert_eq!(*session.next_seq.lock().unwrap(), 3);
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

    session.append(&entries[3..5]).await.unwrap();
    assert_eq!(*session.next_seq.lock().unwrap(), 5);
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
    let (_dir, session, sid) = fresh_session().await;

    // No row exists (fresh subagent whose parent has not written yet): a
    // touch must skip, never fabricate its own row (R3).
    session.touch_meta().await.unwrap();
    assert!(session.audit_meta(&sid).await.unwrap().is_empty());

    // set_title / set_pinned on a row-less session are no-ops too (R3).
    session.set_title(&sid, Some("ghost")).await.unwrap();
    session.set_pinned(&sid, true).await.unwrap();
    assert!(session.audit_meta(&sid).await.unwrap().is_empty());

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

#[tokio::test]
async fn sessions_meta_backfills_parent_link_once() {
    let (_dir, session, sid) = fresh_session().await;

    // 1. build writes the first row with parent = None.
    session
        .create_meta(&sid, Some("model-x"), Some("main"), None, None, None)
        .await
        .unwrap();
    let trail = session.audit_meta(&sid).await.unwrap();
    assert_eq!(trail.len(), 1);
    assert_eq!(trail[0].parent_session_id, None);
    let original_created_at = trail[0].created_at;

    // 2. the parent records the real link → one fresh row is appended, the
    //    old row is kept, and the latest snapshot carries the link.
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
        "preserve created_at"
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
    let (_dir2, session2, sid2) = fresh_session().await;
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
}

#[tokio::test]
async fn sessions_meta_backfills_missing_model_once() {
    let (_dir, session, sid) = fresh_session().await;

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
        "preserve created_at"
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
    assert_eq!(latest.model.as_deref(), Some("model-y"), "first model wins");
}

#[tokio::test]
async fn sessions_meta_set_title_persists_and_survives_touch() {
    let (_dir, session, sid) = fresh_session().await;

    // set_title on a session with no row yet is a no-op Ok (R3).
    session.set_title(&sid, Some("ghost title")).await.unwrap();
    assert!(session.audit_meta(&sid).await.unwrap().is_empty());

    session
        .create_meta(&sid, Some("model-x"), Some("main"), None, None, None)
        .await
        .unwrap();

    // Rename → the list shows the new title, the audit trail appends one
    // snapshot, created_at survives.
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
        "rename preserves columns"
    );
    assert_eq!(
        session.audit_meta(&sid).await.unwrap().len(),
        2,
        "create + rename"
    );

    // touch carries the title: the append-only snapshot semantics mean the
    // newest row still has it.
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
        "touch preserves created_at"
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
    let (_dir, session, sid) = fresh_session().await;

    session.set_pinned(&sid, true).await.unwrap();
    assert!(session.audit_meta(&sid).await.unwrap().is_empty());

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
        "a fresh session reads as never-touched"
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
        "pin preserves columns"
    );
    assert_eq!(
        session.audit_meta(&sid).await.unwrap().len(),
        2,
        "create + pin"
    );

    // touch carries the pin.
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
        "touch preserves created_at"
    );

    // Unpin stores Some(false) — distinct from the None of a never-touched
    // session, both read as unpinned.
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
    let (_dir, session, sid) = fresh_session().await;

    session.set_archived(&sid, true).await.unwrap();
    assert!(session.audit_meta(&sid).await.unwrap().is_empty());

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
        "a fresh session reads as never-touched"
    );

    // Archive → the list shows archived=true, the audit trail appends one
    // snapshot, created_at survives.
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
        "archive preserves columns"
    );
    assert_eq!(
        session.audit_meta(&sid).await.unwrap().len(),
        2,
        "create + archive"
    );

    // touch carries the archived flag.
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
        "touch preserves created_at"
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
async fn sessions_meta_list_is_latest_wins() {
    // Multiple snapshots per session → exactly one list row with the
    // newest fields; distinct sessions each get their own row. Both
    // sessions share one database file and workspace (each store is bound
    // to its own session id, mirroring the Greptime per-session binding).
    let (_dir, path) = temp_db();
    let wid = workspace_id();
    let sid = format!("test-sql-meta-a-{}", crate::session::new_id());
    let sid2 = format!("test-sql-meta-b-{}", crate::session::new_id());
    let p = path.to_str().unwrap();
    let session = SqliteSession::connect(p, &wid, &sid).await.unwrap();
    let session2 = SqliteSession::connect(p, &wid, &sid2).await.unwrap();

    session
        .create_meta(&sid, Some("model-a"), Some("main"), None, None, None)
        .await
        .unwrap();
    session2
        .create_meta(&sid2, Some("model-b"), Some("main"), None, None, None)
        .await
        .unwrap();

    // Overwrite sid's title/pin via set_title/set_pinned (each appends).
    session.set_title(&sid, Some("newest title")).await.unwrap();
    session.set_pinned(&sid, true).await.unwrap();

    let list = session.list_meta().await.unwrap();
    assert_eq!(list.len(), 2, "one row per session");
    // 排序契约（同 Greptime）：list_meta 按 last_active_at 降序（最新在前）。
    // turso 不保证无 ORDER BY 的默认排序，必须显式断言。
    assert!(
        list.windows(2)
            .all(|w| w[0].last_active_at >= w[1].last_active_at),
        "list_meta must be newest-first by last_active_at, got: {:?}",
        list.iter()
            .map(|m| (m.session_id.as_str(), m.last_active_at))
            .collect::<Vec<_>>()
    );
    let mine = list
        .iter()
        .find(|m| m.session_id == sid)
        .expect("session listed");
    assert_eq!(
        mine.title.as_deref(),
        Some("newest title"),
        "latest snapshot wins"
    );
    assert_eq!(mine.pinned, Some(true));
    assert_eq!(
        mine.model.as_deref(),
        Some("model-a"),
        "immutable columns carried"
    );
    let other = list
        .iter()
        .find(|m| m.session_id == sid2)
        .expect("second session listed");
    assert_eq!(other.title, None);
    assert_eq!(other.model.as_deref(), Some("model-b"));

    // Greptime's contract: newest-first by last_active_at (sid was touched
    // after sid2, so its latest snapshot leads the list).
    assert_eq!(
        list[0].session_id, sid,
        "newest-active session listed first"
    );
    assert!(
        list[0].last_active_at >= list[1].last_active_at,
        "list must be ordered newest-first by last_active_at"
    );
}

#[tokio::test]
async fn sessions_meta_list_same_last_active_tie_keeps_both_rows() {
    // Two DIFFERENT sessions whose latest snapshots tie on last_active_at
    // (possible across processes — each process has its own monotonic
    // clock): list_meta must still return exactly one deduped row per
    // session, never dropping either on the tie.
    let (_dir, path) = temp_db();
    let wid = workspace_id();
    let sid_a = format!("test-sql-tie-a-{}", crate::session::new_id());
    let sid_b = format!("test-sql-tie-b-{}", crate::session::new_id());
    let p = path.to_str().unwrap();
    let session = SqliteSession::connect(p, &wid, &sid_a).await.unwrap();

    session
        .create_meta(&sid_a, Some("model-a"), None, None, None, None)
        .await
        .unwrap();
    session
        .create_meta(&sid_b, Some("model-b"), None, None, None, None)
        .await
        .unwrap();

    // Force the tie: stamp both latest snapshots with the SAME
    // last_active_at (the cross-process scenario where two monotonic
    // clocks happen to collide).
    let tie_ts = 1_700_000_000_000_000i64;
    let conn = session.conn.lock().await;
    conn.execute("UPDATE sessions SET last_active_at = ?1", (tie_ts,))
        .await
        .unwrap();
    drop(conn);

    let list = session.list_meta().await.unwrap();
    assert_eq!(list.len(), 2, "tied sessions must both be listed");
    assert_eq!(
        list.iter().filter(|m| m.session_id == sid_a).count(),
        1,
        "exactly one deduped row per session"
    );
    assert_eq!(
        list.iter().filter(|m| m.session_id == sid_b).count(),
        1,
        "exactly one deduped row per session"
    );
    assert!(
        list.iter()
            .all(|m| datetime_to_us(m.last_active_at) == tie_ts),
        "both rows carry the tied timestamp"
    );
    // The newest-first ordering contract still holds (equal timestamps
    // satisfy >= in either order).
    assert!(
        list.windows(2)
            .all(|w| w[0].last_active_at >= w[1].last_active_at),
        "tie must not break newest-first ordering"
    );
    let a = list.iter().find(|m| m.session_id == sid_a).unwrap();
    let b = list.iter().find(|m| m.session_id == sid_b).unwrap();
    assert_eq!(
        a.model.as_deref(),
        Some("model-a"),
        "dedup keeps A's fields"
    );
    assert_eq!(
        b.model.as_deref(),
        Some("model-b"),
        "dedup keeps B's fields"
    );
}

#[tokio::test]
async fn sessions_meta_same_microsecond_insert_collides_loudly() {
    // The SQLite PK carries last_active_at: two snapshots of the SAME
    // session at the same microsecond (only possible across processes —
    // `next_event_time_us` is strictly monotonic within one) must fail
    // loudly instead of silently overwriting, per the table's documented
    // contract.
    let (_dir, session, sid) = fresh_session().await;
    session
        .create_meta(&sid, Some("model-x"), None, None, None, None)
        .await
        .unwrap();

    // Re-insert a row with the SAME (workspace, session, last_active_at).
    let tied_ts = datetime_to_us(session.audit_meta(&sid).await.unwrap()[0].last_active_at);
    let conn = session.conn.lock().await;
    let dup = conn
        .execute(
            "INSERT INTO sessions \
             (workspace_id, session_id, created_at, last_active_at) \
             VALUES (?1, ?2, ?3, ?3)",
            (
                session.workspace_id.as_str(),
                session.session_id.as_str(),
                tied_ts,
            ),
        )
        .await;
    drop(conn);
    assert!(
        dup.is_err(),
        "same-session same-microsecond snapshot must collide on the PK"
    );

    // The failed insert changed nothing: the audit trail still holds the
    // single creation row.
    assert_eq!(
        session.audit_meta(&sid).await.unwrap().len(),
        1,
        "collision must not mutate the audit trail"
    );
}

#[tokio::test]
async fn sessions_meta_backfill_is_idempotent() {
    let (_dir, session, sid) = fresh_session().await;
    let entries = test_entries();

    // Pre-table session: transcript rows, no metadata rows.
    session.append(&entries[..3]).await.unwrap();
    assert!(session.audit_meta(&sid).await.unwrap().is_empty());

    session.backfill_sessions().await.unwrap();
    let list = session.list_meta().await.unwrap();
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
}

#[tokio::test]
async fn sessions_meta_backfill_skips_sessions_with_rows() {
    // A session that already has a meta row is skipped by backfill even
    // though it also has entries.
    let (_dir, session, sid) = fresh_session().await;
    let entries = test_entries();
    session.append(&entries[..2]).await.unwrap();
    session
        .create_meta(&sid, Some("m"), Some("main"), None, None, None)
        .await
        .unwrap();
    session.backfill_sessions().await.unwrap();
    let mine: Vec<_> = session
        .list_meta()
        .await
        .unwrap()
        .into_iter()
        .filter(|m| m.session_id == sid)
        .collect();
    assert_eq!(mine.len(), 1);
    assert_eq!(
        mine[0].model.as_deref(),
        Some("m"),
        "existing row untouched"
    );
    assert_eq!(mine[0].entry_count, 2);
}

// ----------------------------------------------------------------------
// rewrite is a no-op
// ----------------------------------------------------------------------

#[tokio::test]
async fn rewrite_is_a_noop() {
    let (_dir, session, _sid) = fresh_session().await;
    let entries = test_entries();
    session.append(&entries[..2]).await.unwrap();
    session
        .rewrite(&[Message::User {
            content: "replacement".into(),
            images: vec![],
        }
        .into()])
        .await
        .unwrap();
    // The transcript is untouched.
    let loaded = session.load().await.unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0], entries[0]);
}

// ----------------------------------------------------------------------
// 僵尸扫描（server.rs scan_zombie_background_tasks）集成测试
// ----------------------------------------------------------------------

/// 僵尸扫描的汇总注入设计（SQLite 后端全链路）：父会话自己的死 bash 行
/// + 2 个子会话名下的死 delegate 行 → 只往父会话注入一条汇总 Notice
/// （含两个子会话 label 和自己的任务）、子会话 0 条新 entry、死行全部
/// 被消费（label 从侧边栏消失）。
#[tokio::test]
async fn zombie_scan_consumes_dead_rows_and_injects_one_parent_notice() {
    // owner 探测需要导出的 hostname：没有时无法构造「确定已死」的 owner，
    // 扫描会保守跳过——与 running_tasks_owner_column_liveness_probe 相同的
    // 环境分支。
    let Some(hostname) = probeable_hostname() else {
        return;
    };
    let (dir, path) = temp_db();
    let root = dir.path().to_path_buf();
    let wid = derive_workspace_id(&root);
    let backend = crate::config::SessionBackend::Sqlite {
        path: Some(path.to_string_lossy().into_owned()),
    };
    let meta_store = crate::session_store::SessionStore::connect_meta(&backend, &root)
        .await
        .expect("connect meta store");
    let parent = format!("scan-parent-{}", crate::session::new_id());
    let child_a = format!("scan-child-a-{}", crate::session::new_id());
    let child_b = format!("scan-child-b-{}", crate::session::new_id());
    // 会话元数据：父 + 两个子（parent_session_id 指向父）。
    for (sid, parent_id) in [
        (parent.as_str(), None),
        (child_a.as_str(), Some(parent.as_str())),
        (child_b.as_str(), Some(parent.as_str())),
    ] {
        meta_store
            .create_meta(&root, sid, Some("test-model"), None, parent_id, None, None)
            .await
            .expect("create meta");
    }
    // running_tasks 行：父会话自己的死 bash 行（subagent_session_id =
    // NULL）+ 两个子会话的死 delegate 行（subagent_session_id = 子会话，
    // 物理上记在父会话的 session_id 行组下）。
    let parent_session = SqliteSession::connect(path.to_str().unwrap(), &wid, &parent)
        .await
        .expect("connect parent session");
    parent_session
        .record_task_start(&parent, 1, "npm run dev", None, None)
        .await
        .unwrap();
    parent_session
        .record_task_start(&parent, 2, "sleep 100", None, None)
        .await
        .unwrap();
    parent_session
        .record_task_start(&parent, 11, "btw: 分析日志", None, Some(&child_a))
        .await
        .unwrap();
    parent_session
        .record_task_start(&parent, 12, "task: 修 bug", None, Some(&child_b))
        .await
        .unwrap();
    // 把 owner 改成确定已死的 pid@hostname，让扫描的 liveness 探测通过。
    {
        let conn = parent_session.conn.lock().await;
        conn.execute(
            "UPDATE running_tasks SET owner_identity = ?1 WHERE workspace_id = ?2",
            (format!("2000000000@{hostname}#deadbeef"), wid.as_str()),
        )
        .await
        .unwrap();
    }
    drop(parent_session);
    // 跑扫描。
    crate::server::scan_zombie_background_tasks(&meta_store, &backend, &root).await;
    // 父会话：恰好 1 条 Notice，含两个子会话 label 和自己的任务。
    let parent_store = crate::session_store::SessionStore::connect(&backend, &root, &parent)
        .await
        .expect("connect parent for load");
    let loaded = parent_store
        .load(&root, &parent)
        .await
        .expect("load parent");
    let notices: Vec<&str> = loaded
        .entries
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::Notice { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(notices.len(), 1, "父会话只收一条汇总 Notice");
    let text = notices[0];
    assert!(
        text.contains("4 background task(s) still running"),
        "N = 自己的 2 + 两个子会话各 1，got: {text}"
    );
    assert!(text.contains("task 1: npm run dev"), "got: {text}");
    assert!(text.contains("task 2: sleep 100"), "got: {text}");
    assert!(
        text.contains("被杀子会话: "),
        "子会话 label 汇总成一行，got: {text}"
    );
    // label 顺序跟随 list_meta（newest first），不做精确顺序断言。
    assert!(text.contains("btw: 分析日志"), "got: {text}");
    assert!(text.contains("task: 修 bug"), "got: {text}");
    assert!(text.contains('、'), "两个 label 用顿号连接成一行: {text}");
    // 子会话：0 条新 entry（不复活：不 append、不留新内容）。
    for child in [&child_a, &child_b] {
        let store = crate::session_store::SessionStore::connect(&backend, &root, child)
            .await
            .expect("connect child for load");
        let loaded = store.load(&root, child).await.expect("load child");
        assert!(
            loaded.entries.is_empty(),
            "子会话 {child} 不得被注入任何 entry"
        );
    }
    // 死行全部被消费（父会话自己的 + 两个子会话的 delegate 行）。
    let check = SqliteSession::connect(path.to_str().unwrap(), &wid, &parent)
        .await
        .expect("connect for row check");
    let conn = check.conn.lock().await;
    let mut rows = conn
        .query(
            "SELECT COUNT(*) FROM running_tasks WHERE workspace_id = ?1",
            (wid.as_str(),),
        )
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    assert_eq!(
        row.get_value(0).unwrap().as_integer().copied(),
        Some(0),
        "死行必须全部被消费"
    );
}
