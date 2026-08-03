//! JSONL metadata sidecar tests: the `.meta.jsonl` per-session snapshot
//! files that mirror the DB `sessions` audit table on the JSONL backend.
//! Every create/touch/rename/pin/archive appends one full snapshot line;
//! the file tail is the latest snapshot.

use super::*;
use crate::agent::{AssistantMessage, Message};
use crate::session::Session;

fn entries() -> Vec<SessionEntry> {
    vec![
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
    ]
}

fn sidecar(root: &Path, session: &str) -> std::path::PathBuf {
    root.join(".e-agent/sessions")
        .join(format!("{session}.meta.jsonl"))
}

fn sidecar_lines(root: &Path, session: &str) -> usize {
    std::fs::read_to_string(sidecar(root, session))
        .map(|raw| raw.lines().filter(|line| !line.trim().is_empty()).count())
        .unwrap_or(0)
}

/// Parse every snapshot line of a sidecar, oldest first.
fn sidecar_snapshots(root: &Path, session: &str) -> Vec<SessionMeta> {
    std::fs::read_to_string(sidecar(root, session))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<SessionMeta>(line).ok())
        .collect()
}

// ----------------------------------------------------------------------
// create / list
// ----------------------------------------------------------------------

#[tokio::test]
async fn create_meta_then_list_visible() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::Jsonl;
    let sid = format!("meta-create-{}", crate::session::new_id());
    store
        .create_meta(
            temp.path(),
            &sid,
            Some("gpt-x"),
            Some("main"),
            None,
            None,
            Some("delegate label"),
        )
        .await
        .unwrap();
    let list = store.list_meta(temp.path()).await.unwrap();
    assert_eq!(list.len(), 1);
    let meta = &list[0];
    assert_eq!(meta.session_id, sid);
    assert_eq!(meta.model.as_deref(), Some("gpt-x"));
    assert_eq!(meta.role.as_deref(), Some("main"));
    assert_eq!(
        meta.entry_count, 0,
        "fresh session has no transcript entries"
    );
    assert_eq!(
        meta.title.as_deref(),
        Some("delegate label"),
        "a title supplied at creation is recorded"
    );
    assert!(meta.pinned.is_none() && meta.archived.is_none());
    assert_eq!(
        sidecar_lines(temp.path(), &sid),
        1,
        "creation writes exactly one snapshot line"
    );
    assert_eq!(
        meta.writer.as_deref(),
        Some(process_identity()),
        "snapshots are stamped with the writing process"
    );
}

#[tokio::test]
async fn create_meta_title_only_applies_on_fresh_creation_and_is_preserved_on_resume() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::Jsonl;
    let sid = format!("meta-title-{}", crate::session::new_id());
    // Fresh creation with a title.
    store
        .create_meta(
            temp.path(),
            &sid,
            Some("gpt-x"),
            Some("main"),
            None,
            None,
            Some("original title"),
        )
        .await
        .unwrap();
    assert_eq!(sidecar_lines(temp.path(), &sid), 1);

    // Resume (row exists, nothing missing): even a different title is a
    // no-op — the creation title survives and nothing is appended.
    store
        .create_meta(
            temp.path(),
            &sid,
            Some("gpt-x"),
            Some("main"),
            None,
            None,
            Some("resume title"),
        )
        .await
        .unwrap();
    assert_eq!(
        sidecar_lines(temp.path(), &sid),
        1,
        "resume must not append a snapshot or rewrite the title"
    );
    let list = store.list_meta(temp.path()).await.unwrap();
    assert_eq!(
        list[0].title.as_deref(),
        Some("original title"),
        "resume keeps the creation title"
    );

    // No title at creation ⇒ the session stays unnamed.
    let other = format!("meta-title-none-{}", crate::session::new_id());
    store
        .create_meta(temp.path(), &other, Some("gpt-x"), None, None, None, None)
        .await
        .unwrap();
    let list = store.list_meta(temp.path()).await.unwrap();
    let other_meta = list
        .iter()
        .find(|m| m.session_id == other)
        .expect("other session listed");
    assert_eq!(other_meta.title, None, "title None ⇒ unnamed");
}

#[tokio::test]
async fn create_meta_records_parent_links_for_subagents() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::Jsonl;
    let parent = format!("meta-parent-{}", crate::session::new_id());
    let sub = format!("sub-{}", crate::session::new_id());
    store
        .create_meta(
            temp.path(),
            &sub,
            Some("gpt-x"),
            Some("delegate"),
            Some(&parent),
            Some(7),
            None,
        )
        .await
        .unwrap();
    let list = store.list_meta(temp.path()).await.unwrap();
    assert_eq!(list[0].parent_session_id.as_deref(), Some(parent.as_str()));
    assert_eq!(list[0].parent_task_id, Some(7));
}

#[tokio::test]
async fn create_meta_is_idempotent_resume() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::Jsonl;
    let sid = format!("meta-idem-{}", crate::session::new_id());
    store
        .create_meta(
            temp.path(),
            &sid,
            Some("model-a"),
            Some("main"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    // Resume with a different model: no second line, created_at untouched.
    store
        .create_meta(
            temp.path(),
            &sid,
            Some("model-b"),
            Some("main"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        sidecar_lines(temp.path(), &sid),
        1,
        "resume must not append"
    );
    let list = store.list_meta(temp.path()).await.unwrap();
    assert_eq!(
        list[0].model.as_deref(),
        Some("model-a"),
        "resume must not rewrite the snapshot"
    );
}

#[tokio::test]
async fn create_meta_after_transcript_counts_existing_entries() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::Jsonl;
    let sid = format!("meta-count-{}", crate::session::new_id());
    Session::append(temp.path(), &sid, &entries()).unwrap();
    Session::append(temp.path(), &sid, &entries()).unwrap();
    store
        .create_meta(
            temp.path(),
            &sid,
            Some("gpt-x"),
            Some("main"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let list = store.list_meta(temp.path()).await.unwrap();
    assert_eq!(list[0].entry_count, 2 * entries().len() as i64);
}

/// DB parity (session_sqlite create_meta): when a row/sidecar already
/// exists and the caller supplies a `model` or `parent_session_id` the
/// existing snapshot lacks (e.g. a `backfill_sessions`-created first line
/// has `model = None`, or a subagent row was written without its parent
/// link), one fresh snapshot is appended carrying the missing fields in —
/// preserving `created_at` and every existing field, with a new
/// `last_active_at`. A later call finds the field set and appends nothing.
#[tokio::test]
async fn create_meta_backfills_missing_model_and_parent_link() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::Jsonl;
    let sid = format!("meta-backfill-{}", crate::session::new_id());
    let parent = format!("meta-backfill-parent-{}", crate::session::new_id());

    // First line like backfill_sessions would write it: no model, no parent.
    store
        .create_meta(temp.path(), &sid, None, None, None, None, None)
        .await
        .unwrap();
    let created = store.list_meta(temp.path()).await.unwrap();
    let created_at = created[0].created_at;
    assert_eq!(created[0].model, None);

    // Resume with the model (SessionFactory::build): one backfill line.
    std::thread::sleep(std::time::Duration::from_millis(5));
    store
        .create_meta(
            temp.path(),
            &sid,
            Some("gpt-x"),
            Some("main"),
            Some(&parent),
            Some(7),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        sidecar_lines(temp.path(), &sid),
        2,
        "missing model + parent → exactly one backfill line"
    );
    let list = store.list_meta(temp.path()).await.unwrap();
    assert_eq!(list[0].model.as_deref(), Some("gpt-x"), "model filled in");
    assert_eq!(list[0].role.as_deref(), Some("main"), "role filled in");
    assert_eq!(
        list[0].parent_session_id.as_deref(),
        Some(parent.as_str()),
        "parent link filled in"
    );
    assert_eq!(list[0].parent_task_id, Some(7));
    assert_eq!(
        list[0].created_at, created_at,
        "created_at is never rewritten by the backfill"
    );
    assert!(
        list[0].last_active_at > created_at,
        "backfill line carries a fresh last_active_at"
    );

    // A third call with a *different* model: the field is already set, so
    // nothing is appended (backfill exactly once).
    store
        .create_meta(
            temp.path(),
            &sid,
            Some("gpt-y"),
            Some("other"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(sidecar_lines(temp.path(), &sid), 2, "backfill happens once");
    let list = store.list_meta(temp.path()).await.unwrap();
    assert_eq!(
        list[0].model.as_deref(),
        Some("gpt-x"),
        "existing model is never overwritten"
    );

    // A bare parent_task_id without parent_session_id must NOT trigger a
    // backfill (DB gate: only parent_session_id / model count).
    let other = format!("meta-backfill-only-task-{}", crate::session::new_id());
    store
        .create_meta(temp.path(), &other, None, None, None, None, None)
        .await
        .unwrap();
    store
        .create_meta(temp.path(), &other, None, None, None, Some(9), None)
        .await
        .unwrap();
    assert_eq!(
        sidecar_lines(temp.path(), &other),
        1,
        "parent_task_id alone does not trigger a backfill"
    );
}

/// The backfill snapshot is a full snapshot: user fields and immutable
/// columns set before the backfill survive it.
#[tokio::test]
async fn create_meta_backfill_preserves_existing_user_fields() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::Jsonl;
    let sid = format!("meta-backfill-fields-{}", crate::session::new_id());
    store
        .create_meta(temp.path(), &sid, None, None, None, None, None)
        .await
        .unwrap();
    store
        .set_title(temp.path(), &sid, Some("named"))
        .await
        .unwrap();
    store.set_pinned(temp.path(), &sid, true).await.unwrap();
    let before = store.list_meta(temp.path()).await.unwrap();
    assert_eq!(before[0].model, None);

    store
        .create_meta(
            temp.path(),
            &sid,
            Some("gpt-x"),
            Some("main"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let list = store.list_meta(temp.path()).await.unwrap();
    assert_eq!(list[0].model.as_deref(), Some("gpt-x"));
    assert_eq!(
        list[0].title.as_deref(),
        Some("named"),
        "title carried over"
    );
    assert_eq!(list[0].pinned, Some(true), "pin carried over");
    assert_eq!(list[0].archived, None);
    assert_eq!(
        sidecar_lines(temp.path(), &sid),
        4,
        "create + title + pin + backfill"
    );
}

// ----------------------------------------------------------------------
// touch
// ----------------------------------------------------------------------

#[tokio::test]
async fn touch_updates_last_active_and_entry_count() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::Jsonl;
    let sid = format!("meta-touch-{}", crate::session::new_id());

    // No sidecar yet: touch must be a no-op (R3) and must not create the file.
    store.touch_meta(temp.path(), &sid);
    assert!(
        !sidecar(temp.path(), &sid).exists(),
        "R3: touch never self-creates"
    );

    Session::append(temp.path(), &sid, &entries()).unwrap();
    store
        .create_meta(
            temp.path(),
            &sid,
            Some("gpt-x"),
            Some("main"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let before = store.list_meta(temp.path()).await.unwrap();
    assert_eq!(before[0].entry_count, entries().len() as i64);

    store.touch_meta(temp.path(), &sid);
    let list = store.list_meta(temp.path()).await.unwrap();
    assert_eq!(
        list[0].entry_count,
        entries().len() as i64,
        "touch refreshes entry_count from the transcript"
    );
    assert!(
        list[0].last_active_at >= before[0].last_active_at,
        "touch bumps last_active_at"
    );
    assert_eq!(
        sidecar_lines(temp.path(), &sid),
        2,
        "touch appends exactly one snapshot line"
    );

    // More entries → the next touch carries the new count.
    Session::append(temp.path(), &sid, &entries()).unwrap();
    store.touch_meta(temp.path(), &sid);
    let list = store.list_meta(temp.path()).await.unwrap();
    assert_eq!(list[0].entry_count, 2 * entries().len() as i64);
    assert_eq!(sidecar_lines(temp.path(), &sid), 3);
}

// ----------------------------------------------------------------------
// title / pin / archive
// ----------------------------------------------------------------------

#[tokio::test]
async fn set_title_pin_archive_append_and_list_reflects() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::Jsonl;
    let sid = format!("meta-flags-{}", crate::session::new_id());
    store
        .create_meta(
            temp.path(),
            &sid,
            Some("gpt-x"),
            Some("main"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    store
        .set_title(temp.path(), &sid, Some("my chat"))
        .await
        .unwrap();
    store.set_pinned(temp.path(), &sid, true).await.unwrap();
    store.set_archived(temp.path(), &sid, true).await.unwrap();
    assert_eq!(
        sidecar_lines(temp.path(), &sid),
        4,
        "each mutation appends one full snapshot"
    );

    let list = store.list_meta(temp.path()).await.unwrap();
    let meta = &list[0];
    assert_eq!(meta.title.as_deref(), Some("my chat"));
    assert_eq!(meta.pinned, Some(true));
    assert_eq!(meta.archived, Some(true));

    // Clear the title / unpin / restore: fresh snapshots again.
    store.set_title(temp.path(), &sid, None).await.unwrap();
    store.set_pinned(temp.path(), &sid, false).await.unwrap();
    store.set_archived(temp.path(), &sid, false).await.unwrap();
    let list = store.list_meta(temp.path()).await.unwrap();
    assert_eq!(list[0].title, None);
    assert_eq!(
        list[0].pinned,
        Some(false),
        "explicitly unpinned, not never-touched"
    );
    assert_eq!(
        list[0].archived,
        Some(false),
        "explicitly restored, not never-touched"
    );
    assert_eq!(sidecar_lines(temp.path(), &sid), 7);
}

#[tokio::test]
async fn snapshots_carry_full_state_no_partial_rows() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::Jsonl;
    let sid = format!("meta-full-{}", crate::session::new_id());
    Session::append(temp.path(), &sid, &entries()).unwrap();
    store
        .create_meta(
            temp.path(),
            &sid,
            Some("gpt-x"),
            Some("main"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    store.set_title(temp.path(), &sid, Some("t")).await.unwrap();
    store.set_pinned(temp.path(), &sid, true).await.unwrap();
    store.set_archived(temp.path(), &sid, true).await.unwrap();
    store.touch_meta(temp.path(), &sid);

    let snapshots = sidecar_snapshots(temp.path(), &sid);
    assert_eq!(snapshots.len(), 5);
    let created_at = snapshots[0].created_at;
    let list = store.list_meta(temp.path()).await.unwrap();
    let tail = &list[0];
    assert_eq!(tail.title.as_deref(), Some("t"));
    assert_eq!(tail.pinned, Some(true));
    assert_eq!(tail.archived, Some(true));
    assert_eq!(
        tail.model.as_deref(),
        Some("gpt-x"),
        "immutable columns survive touches"
    );
    assert_eq!(tail.created_at, created_at, "created_at is never rewritten");
    assert_eq!(tail.writer.as_deref(), Some(process_identity()));
    for snapshot in &snapshots {
        assert_eq!(snapshot.session_id, sid);
        assert_eq!(
            snapshot.created_at, created_at,
            "every snapshot is complete"
        );
    }
}

/// DB parity (insert_meta): every APPENDED snapshot is stamped with the
/// writing process's identity at append time — a snapshot carried forward
/// from an earlier line (created by another process) never replays its
/// stale writer. The historical line keeps its own writer; each new line
/// gets the current process identity.
#[tokio::test]
async fn writer_is_refreshed_on_every_append() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::Jsonl;
    let sid = format!("meta-writer-{}", crate::session::new_id());
    store
        .create_meta(temp.path(), &sid, None, None, None, None, None)
        .await
        .unwrap();

    // Simulate a sidecar whose tail was written by another process: rewrite
    // the first line with a foreign writer, then append from this process.
    let mut snapshots = sidecar_snapshots(temp.path(), &sid);
    assert_eq!(snapshots.len(), 1);
    snapshots[0].writer = Some("other@host#deadbeef".into());
    let raw = snapshots
        .iter()
        .map(|meta| serde_json::to_string(meta).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(sidecar(temp.path(), &sid), format!("{raw}\n")).unwrap();

    store.touch_meta(temp.path(), &sid);
    store.set_title(temp.path(), &sid, Some("t")).await.unwrap();
    store.set_archived(temp.path(), &sid, true).await.unwrap();
    store
        .create_meta(
            temp.path(),
            &sid,
            Some("gpt-x"),
            Some("main"),
            None,
            None,
            None,
        )
        .await
        .unwrap(); // model was None → backfill path, also stamps fresh

    let snapshots = sidecar_snapshots(temp.path(), &sid);
    assert_eq!(
        snapshots.len(),
        5,
        "1 foreign + touch + title + archive + backfill"
    );
    assert_eq!(
        snapshots[0].writer.as_deref(),
        Some("other@host#deadbeef"),
        "the historical line keeps its own writer"
    );
    for snapshot in &snapshots[1..] {
        assert_eq!(
            snapshot.writer.as_deref(),
            Some(process_identity()),
            "every appended snapshot is stamped by the writing process"
        );
    }
    let list = store.list_meta(temp.path()).await.unwrap();
    assert_eq!(
        list[0].writer.as_deref(),
        Some(process_identity()),
        "the tail (list view) carries the current process identity"
    );
}

#[tokio::test]
async fn flag_setters_without_sidecar_are_noops() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::Jsonl;
    let sid = format!("meta-noop-{}", crate::session::new_id());
    Session::append(temp.path(), &sid, &entries()).unwrap(); // transcript but no sidecar
    store
        .set_title(temp.path(), &sid, Some("nope"))
        .await
        .unwrap();
    store.set_pinned(temp.path(), &sid, true).await.unwrap();
    store.set_archived(temp.path(), &sid, true).await.unwrap();
    assert!(
        !sidecar(temp.path(), &sid).exists(),
        "R3: setters never self-create"
    );
    assert!(store.list_meta(temp.path()).await.unwrap().is_empty());
}

// ----------------------------------------------------------------------
// delete
// ----------------------------------------------------------------------

#[tokio::test]
async fn delete_meta_hides_from_list_keeps_transcript() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::Jsonl;
    let sid = format!("meta-del-{}", crate::session::new_id());
    Session::append(temp.path(), &sid, &entries()).unwrap();
    store
        .create_meta(
            temp.path(),
            &sid,
            Some("gpt-x"),
            Some("main"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(store.list_meta(temp.path()).await.unwrap().len(), 1);

    store.delete_meta(temp.path(), &sid).await.unwrap();
    assert!(
        store.list_meta(temp.path()).await.unwrap().is_empty(),
        "deleted session vanishes from the list"
    );
    assert!(!sidecar(temp.path(), &sid).exists());
    // The transcript survives → resume still works.
    assert_eq!(Session::load(temp.path(), &sid).unwrap().entries, entries());

    // Idempotent: deleting a missing sidecar is Ok (zero-row DELETE).
    store.delete_meta(temp.path(), &sid).await.unwrap();
}

// ----------------------------------------------------------------------
// list scan rules
// ----------------------------------------------------------------------

#[tokio::test]
async fn list_scans_sidecars_only_and_ignores_background_records() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::Jsonl;
    let a = format!("meta-a-{}", crate::session::new_id());
    let b = format!("meta-b-{}", crate::session::new_id());
    Session::append(temp.path(), &a, &entries()).unwrap();
    Session::append(temp.path(), &b, &entries()).unwrap();
    store
        .create_meta(temp.path(), &a, None, None, None, None, None)
        .await
        .unwrap();
    // b has a transcript but no sidecar → skipped (listing stays read-only;
    // backfill_sessions is the dedicated migration).
    let list = store.list_meta(temp.path()).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].session_id, a);

    // A sidecar with no transcript (a fresh zero-turn session) IS listed —
    // the sidecar is the sessions-table mirror, and the DB lists a row
    // whose session_entries are still empty too.
    store
        .create_meta(temp.path(), "orphan-xyz", None, None, None, None, None)
        .await
        .unwrap();
    let list = store.list_meta(temp.path()).await.unwrap();
    assert_eq!(
        list.len(),
        2,
        "sidecar without transcript is still a session"
    );
    assert!(list.iter().any(|m| m.session_id == "orphan-xyz"));

    // Background record files are never sessions.
    Session::record_background_start(temp.path(), &a, 1, "build", None).unwrap();
    let list = store.list_meta(temp.path()).await.unwrap();
    assert_eq!(list.len(), 2, "background records must not be listed");
    assert!(list.iter().any(|m| m.session_id == a));
    assert!(list.iter().any(|m| m.session_id == "orphan-xyz"));
}

#[tokio::test]
async fn list_meta_sorts_newest_activity_first() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::Jsonl;
    let a = format!("meta-sort-a-{}", crate::session::new_id());
    let b = format!("meta-sort-b-{}", crate::session::new_id());
    store
        .create_meta(temp.path(), &a, None, None, None, None, None)
        .await
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    store
        .create_meta(temp.path(), &b, None, None, None, None, None)
        .await
        .unwrap();
    let list = store.list_meta(temp.path()).await.unwrap();
    assert_eq!(list[0].session_id, b, "newest activity first");
    assert_eq!(list[1].session_id, a);

    // Touching the older one brings it to the front.
    store.touch_meta(temp.path(), &a);
    let list = store.list_meta(temp.path()).await.unwrap();
    assert_eq!(list[0].session_id, a);
    assert_eq!(list[1].session_id, b);
}

// ----------------------------------------------------------------------
// backfill
// ----------------------------------------------------------------------

#[tokio::test]
async fn backfill_creates_first_lines_for_sidecar_less_transcripts() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::Jsonl;
    let old = format!("meta-old-{}", crate::session::new_id());
    let fresh = format!("meta-fresh-{}", crate::session::new_id());
    Session::append(temp.path(), &old, &entries()).unwrap();
    Session::append(temp.path(), &old, &entries()).unwrap();
    store
        .create_meta(
            temp.path(),
            &fresh,
            Some("gpt-x"),
            Some("main"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let fresh_before = std::fs::read_to_string(sidecar(temp.path(), &fresh)).unwrap();

    store.backfill_sessions(temp.path()).await.unwrap();

    // The old transcript got a first line carrying its entry count; the
    // sidecar has no model/role (unknowable from the transcript).
    let list = store.list_meta(temp.path()).await.unwrap();
    let old_meta = list
        .iter()
        .find(|m| m.session_id == old)
        .expect("backfilled session is listed");
    assert_eq!(old_meta.entry_count, 2 * entries().len() as i64);
    assert_eq!(old_meta.model, None);
    assert_eq!(sidecar_lines(temp.path(), &old), 1);

    // The fresh session is untouched (idempotent; created_at preserved).
    assert_eq!(
        std::fs::read_to_string(sidecar(temp.path(), &fresh)).unwrap(),
        fresh_before
    );

    // Running backfill again adds nothing.
    store.backfill_sessions(temp.path()).await.unwrap();
    assert_eq!(sidecar_lines(temp.path(), &old), 1);
    assert_eq!(store.list_meta(temp.path()).await.unwrap().len(), 2);
}

// ----------------------------------------------------------------------
// label_for_subagent (background record scan)
// ----------------------------------------------------------------------

#[tokio::test]
async fn label_for_subagent_reads_background_records() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::Jsonl;
    let parent = format!("meta-parent-{}", crate::session::new_id());
    let sub = format!("sub-{}", crate::session::new_id());

    // No surviving records → None, exactly like a consumed running_tasks row.
    assert_eq!(
        store.label_for_subagent(temp.path(), &sub).await.unwrap(),
        None
    );

    // A surviving delegate record with a matching session_id → its label.
    Session::record_background_start(temp.path(), &parent, 1, "delegate: draft plan", Some(&sub))
        .unwrap();
    assert_eq!(
        store
            .label_for_subagent(temp.path(), &sub)
            .await
            .unwrap()
            .as_deref(),
        Some("delegate: draft plan")
    );

    // Non-matching records (other subagents, plain bash) are ignored.
    let other_sub = format!("sub-{}", crate::session::new_id());
    Session::record_background_start(temp.path(), &parent, 2, "other task", Some(&other_sub))
        .unwrap();
    Session::record_background_start(temp.path(), &parent, 3, "plain bash", None).unwrap();
    assert_eq!(
        store
            .label_for_subagent(temp.path(), &sub)
            .await
            .unwrap()
            .as_deref(),
        Some("delegate: draft plan"),
        "non-matching records must not shadow the match"
    );

    // A later record for the same subagent wins (newest label).
    Session::record_background_start(temp.path(), &parent, 4, "delegate: revise plan", Some(&sub))
        .unwrap();
    assert_eq!(
        store
            .label_for_subagent(temp.path(), &sub)
            .await
            .unwrap()
            .as_deref(),
        Some("delegate: revise plan")
    );

    // Consuming the records (delegate tasks completed) → None again.
    Session::clear_background_task(temp.path(), &parent, 1);
    Session::clear_background_task(temp.path(), &parent, 4);
    assert_eq!(
        store.label_for_subagent(temp.path(), &sub).await.unwrap(),
        None
    );

    // The lookup is non-destructive: other records survive untouched.
    assert_eq!(
        store
            .label_for_subagent(temp.path(), &other_sub)
            .await
            .unwrap()
            .as_deref(),
        Some("other task")
    );
}

// ----------------------------------------------------------------------
// file discipline
// ----------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn sidecar_files_are_private_0600() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::Jsonl;
    let sid = format!("meta-mode-{}", crate::session::new_id());
    store
        .create_meta(temp.path(), &sid, None, None, None, None, None)
        .await
        .unwrap();
    let mode = std::fs::metadata(sidecar(temp.path(), &sid))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[tokio::test]
async fn invalid_session_names_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::Jsonl;
    assert!(
        store
            .create_meta(temp.path(), "../escape", None, None, None, None, None)
            .await
            .is_err()
    );
    assert!(
        store
            .set_title(temp.path(), "a.b", Some("x"))
            .await
            .is_err()
    );
    assert!(store.delete_meta(temp.path(), "").await.is_err());
}
