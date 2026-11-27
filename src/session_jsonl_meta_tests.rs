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

/// An existing sidecar that holds no decodable record (a zero-byte file, or
/// a lone torn line) is UNKNOWN provenance, so a creation call must not
/// append a parentless first line over it: the next read would see only
/// that clean record and call the session parentless. A fresh sidecar
/// (`NotFound`) still gets its first line, and so does one created after
/// the damaged file is actually removed (the real repair).
#[tokio::test]
async fn create_meta_never_writes_a_first_line_over_an_undecodable_sidecar() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::Jsonl;

    // Zero-byte sidecar: the crash window between file creation and the
    // first append.
    let empty = format!("meta-empty-{}", crate::session::new_id());
    std::fs::create_dir_all(sidecar(temp.path(), &empty).parent().unwrap()).unwrap();
    std::fs::write(sidecar(temp.path(), &empty), "").unwrap();
    store
        .create_meta(
            temp.path(),
            &empty,
            Some("gpt-x"),
            Some("subagent"),
            Some("parent-x"),
            Some(1),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(sidecar(temp.path(), &empty)).unwrap(),
        b"",
        "a zero-byte sidecar is never turned into a clean parentless record"
    );

    // A lone torn tail is the same unknown state: nothing to backfill from,
    // so nothing may be appended.
    let torn = format!("meta-torn-only-{}", crate::session::new_id());
    std::fs::write(sidecar(temp.path(), &torn), b"{\"session_id\":").unwrap();
    store
        .create_meta(
            temp.path(),
            &torn,
            Some("gpt-x"),
            Some("subagent"),
            Some("parent-x"),
            Some(1),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(sidecar(temp.path(), &torn)).unwrap(),
        b"{\"session_id\":",
        "a torn only record is never turned into a clean parentless record"
    );

    // The bootstrap backfill follows the same rule for a transcript whose
    // sidecar is zero-byte: skip it, do not manufacture a first line.
    Session::append(temp.path(), &empty, &entries()).unwrap();
    store.backfill_sessions(temp.path()).await.unwrap();
    assert_eq!(
        std::fs::read(sidecar(temp.path(), &empty)).unwrap(),
        b"",
        "backfill leaves an interrupted first write alone"
    );

    // A sidecar that does not exist is still created normally.
    let fresh = format!("meta-fresh-{}", crate::session::new_id());
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
    assert_eq!(
        sidecar_lines(temp.path(), &fresh),
        1,
        "a fresh sidecar gets exactly one first record"
    );

    // And a real repair (the damaged file removed) lets creation proceed.
    std::fs::remove_file(sidecar(temp.path(), &empty)).unwrap();
    store
        .create_meta(
            temp.path(),
            &empty,
            Some("gpt-x"),
            Some("subagent"),
            Some("parent-x"),
            Some(1),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        sidecar_lines(temp.path(), &empty),
        1,
        "creation writes once the unknown sidecar is repaired"
    );
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
// sidebar filter (`?exclude=archived_children` pushdown)
// ----------------------------------------------------------------------

/// `list_meta_with_diagnostics` with the sidebar filter active returns only
/// pinned sessions, unarchived roots, and live sessions — the same rule the
/// SQL backends push into their current-snapshot query. `None` stays the
/// unfiltered list.
#[tokio::test]
async fn sidebar_filter_keeps_pinned_roots_and_live_children() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::Jsonl;
    for (id, parent) in [
        ("keep-main", None),
        ("arch-main", None),
        ("pin-arch", None),
        ("idle-child", Some("keep-main")),
        ("live-child", Some("keep-main")),
    ] {
        store
            .create_meta(temp.path(), id, None, None, parent, None, None)
            .await
            .unwrap();
    }
    store
        .set_archived(temp.path(), "arch-main", true)
        .await
        .unwrap();
    store
        .set_archived(temp.path(), "pin-arch", true)
        .await
        .unwrap();
    store
        .set_pinned(temp.path(), "pin-arch", true)
        .await
        .unwrap();

    // Unfiltered: every sidecar is listed.
    let (all, _) = store
        .list_meta_with_diagnostics(temp.path(), None)
        .await
        .unwrap();
    assert_eq!(all.len(), 5);
    assert_eq!(
        all.iter()
            .find(|m| m.session_id == "arch-main")
            .and_then(|m| m.archived),
        Some(true),
        "the latest snapshot carries the archive flag"
    );

    // Live child survives even though its `active` flag is only merged in
    // after this backend filter runs; the inactive child and the archived
    // unpinned session are gone.
    let live: HashSet<String> = ["live-child".to_owned()].into_iter().collect();
    let (rows, diagnostic) = store
        .list_meta_with_diagnostics(temp.path(), Some(&live))
        .await
        .unwrap();
    let mut ids: Vec<&str> = rows.iter().map(|m| m.session_id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, ["keep-main", "live-child", "pin-arch"]);
    assert_eq!(diagnostic.logical_rows, 3);

    // Empty live set: only pinned / unarchived roots survive.
    let empty = HashSet::new();
    let (rows, _) = store
        .list_meta_with_diagnostics(temp.path(), Some(&empty))
        .await
        .unwrap();
    let mut ids: Vec<&str> = rows.iter().map(|m| m.session_id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, ["keep-main", "pin-arch"]);
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
    Session::record_background_start(temp.path(), &a, 1, "build", None, None).unwrap();
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
    Session::record_background_start(
        temp.path(),
        &parent,
        1,
        "delegate: draft plan",
        None,
        Some(&sub),
    )
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
    Session::record_background_start(
        temp.path(),
        &parent,
        2,
        "other task",
        None,
        Some(&other_sub),
    )
    .unwrap();
    Session::record_background_start(temp.path(), &parent, 3, "plain bash", None, None).unwrap();
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
    Session::record_background_start(
        temp.path(),
        &parent,
        4,
        "delegate: revise plan",
        None,
        Some(&sub),
    )
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

// ----------------------------------------------------------------------
// refs (PR / issue links)
// ----------------------------------------------------------------------

/// The single most dangerous compatibility case: a sidecar line written
/// BEFORE the `refs` field existed. The tail-read parses each line as
/// `SessionMeta` and silently skips unparseable lines, so a missing
/// `#[serde(default)]` would hide every pre-existing session from
/// `list_meta`. The session must list, with empty lists.
#[tokio::test]
async fn meta_line_without_refs_field_lists_as_empty() {
    let temp = tempfile::tempdir().unwrap();
    let sid = format!("meta-legacy-{}", crate::session::new_id());
    let path = sidecar(temp.path(), &sid);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // Build the exact snapshot the pre-change writer emitted, then drop the
    // new key: every other field (including `label`) stays byte-identical to
    // a real legacy line, so this cannot drift from the real record shape.
    let now = chrono::Utc::now().naive_utc();
    let mut legacy = serde_json::to_value(SessionMeta {
        session_id: sid.clone(),
        created_at: now,
        last_active_at: now,
        model: Some("gpt-x".into()),
        role: Some("main".into()),
        entry_count: 3,
        parent_session_id: None,
        parent_task_id: None,
        title: Some("legacy".into()),
        pinned: None,
        archived: None,
        refs: Vec::new(),
        writer: Some("1@host#abc".into()),
        label: None,
    })
    .unwrap();
    legacy
        .as_object_mut()
        .expect("snapshot is an object")
        .remove("refs");
    std::fs::write(&path, format!("{legacy}\n")).unwrap();

    let store = SessionStore::Jsonl;
    let list = store.list_meta(temp.path()).await.unwrap();
    let meta = list
        .iter()
        .find(|meta| meta.session_id == sid)
        .expect("a pre-refs snapshot must still list (serde default, not a skip)");
    assert_eq!(meta.title.as_deref(), Some("legacy"));
    assert!(meta.refs.is_empty(), "absent refs read as empty");
}

/// Both kinds round-trip through the sidecar as ONE JSON value, deduplicate by
/// exact URL within a kind, and removing leaves the other kind intact.
#[tokio::test]
async fn refs_round_trip_through_the_sidecar() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::Jsonl;
    let sid = format!("meta-refs-{}", crate::session::new_id());
    store
        .create_meta(temp.path(), &sid, None, None, None, None, None)
        .await
        .unwrap();

    let pr = SessionRef::new("pr", "https://github.com/acme/widget/pull/14").unwrap();
    let issue = SessionRef::new("issue", "https://github.com/acme/widget/issues/22").unwrap();
    let stored = store
        .update_refs(temp.path(), &sid, |refs| {
            refs.push(pr.clone());
            refs.push(issue.clone());
            refs.push(pr.clone()); // duplicate collapses
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(stored, vec![pr.clone(), issue.clone()]);
    let listed = store.list_meta(temp.path()).await.unwrap();
    assert_eq!(listed[0].refs, vec![pr.clone(), issue.clone()]);

    // One JSON value on the row: the tail line carries `refs` as an array of
    // {"kind","url"} objects (a future kind needs no migration).
    let tail = std::fs::read_to_string(sidecar(temp.path(), &sid)).unwrap();
    let last = tail.lines().rfind(|l| !l.trim().is_empty()).unwrap();
    let value: serde_json::Value = serde_json::from_str(last).unwrap();
    assert_eq!(
        value["refs"],
        serde_json::json!([
            {"kind": "pr", "url": "https://github.com/acme/widget/pull/14"},
            {"kind": "issue", "url": "https://github.com/acme/widget/issues/22"}
        ])
    );

    let after_remove = store
        .update_refs(temp.path(), &sid, |refs| {
            refs.retain(|link| link != &pr);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(after_remove, vec![issue]);
    assert_eq!(
        store.list_meta(temp.path()).await.unwrap()[0].refs,
        after_remove
    );
}

/// A link can only be stored when the session has a metadata row: silently
/// returning an empty list would look like success and lose the link.
#[tokio::test]
async fn update_refs_without_a_meta_row_fails_loudly() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::Jsonl;
    let error = store
        .update_refs(temp.path(), "meta-refs-ghost", |refs| {
            refs.push(SessionRef::new("pr", "https://github.com/a/b/pull/1").unwrap());
            Ok(())
        })
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("no metadata row"),
        "{error:#}"
    );
}

// ----------------------------------------------------------------------
// concurrent snapshot mutations
// ----------------------------------------------------------------------

/// Concurrent ref edits must ALL survive: the sidecar is read-mutate-append,
/// so every per-session metadata writer takes one snapshot lock. Sixteen
/// simultaneous adds released by a barrier make the lost update certain on
/// the unsynchronized code (each writer reads the same newest line and the
/// last append wins); with the lock every URL is stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_ref_edits_all_survive() {
    const WRITERS: usize = 16;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    let session = format!("refs-race-{}", crate::session::new_id());
    let store = SessionStore::Jsonl;
    store
        .create_meta(&root, &session, Some("m"), Some("main"), None, None, None)
        .await
        .unwrap();

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(WRITERS));
    std::thread::scope(|scope| {
        for writer in 0..WRITERS {
            let root = root.clone();
            let session = session.clone();
            let barrier = barrier.clone();
            scope.spawn(move || {
                let url = format!("https://github.com/acme/widget/pull/{writer}");
                let _ = barrier.wait();
                jsonl_update_refs(&root, &session, |refs| {
                    refs.push(SessionRef::new("pr", &url).unwrap());
                    Ok(())
                })
                .expect("the edit is stored");
            });
        }
    });

    let stored = jsonl_read_meta_snapshot(&root, &session)
        .unwrap()
        .expect("sidecar")
        .refs;
    assert_eq!(
        stored.len(),
        WRITERS,
        "every concurrent add must survive: {stored:?}"
    );
    let listed = store.list_meta(&root).await.unwrap();
    assert_eq!(
        listed
            .iter()
            .find(|meta| meta.session_id == session)
            .expect("listed session")
            .refs
            .len(),
        WRITERS,
        "the list view sees every stored link too"
    );
}

/// A metadata write waits for the per-session snapshot lock: while one
/// writer holds it, another cannot read-mutate-append (the deterministic
/// mechanism behind the concurrent-add test above).
#[tokio::test]
async fn metadata_writes_wait_for_the_per_session_lock() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    let session = format!("refs-lock-{}", crate::session::new_id());
    let store = SessionStore::Jsonl;
    store
        .create_meta(&root, &session, Some("m"), Some("main"), None, None, None)
        .await
        .unwrap();
    let path = jsonl_meta_sidecar_path(&root, &session).unwrap();

    let started = AtomicBool::new(false);
    let finished = AtomicBool::new(false);
    let guard = lock_jsonl_meta(&path);
    std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            started.store(true, Ordering::SeqCst);
            jsonl_update_refs(&root, &session, |refs| {
                refs.push(SessionRef::new("pr", "https://github.com/a/b/pull/1").unwrap());
                Ok(())
            })
            .expect("the edit is stored");
            finished.store(true, Ordering::SeqCst);
        });
        while !started.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !finished.load(Ordering::SeqCst),
            "a second writer must not read-mutate-append while the lock is held"
        );
        drop(guard);
        writer.join().unwrap();
    });
    assert!(finished.load(Ordering::SeqCst));
    assert!(
        jsonl_read_meta_snapshot(&root, &session)
            .unwrap()
            .unwrap()
            .refs
            .len()
            == 1
    );
}
