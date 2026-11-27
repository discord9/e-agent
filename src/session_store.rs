//! Runtime session backend dispatch. A simple enum chooses between the
//! default JSONL file backend and the optional GreptimeDB / SQLite backends
//! without introducing a trait (see AGENTS.md: "one adapter, no seam").
//!
//! Each variant holds whatever state its backend needs:
//!
//! - **Jsonl** — stateless marker; every call provides `root` + `name`.
//! - **Greptime** — a connected + session-bound client shared by immutable
//!   references; tokio-postgres provides concurrent query/write dispatch.
//! - **Sqlite** — a connected + session-bound client behind a Mutex, same
//!   shape as Greptime; the session id is bound at connect time and the
//!   database file is shared across sessions.
//!
//! Background-task state (`*.background.jsonl` files on JSONL, the
//! `running_tasks` table on Greptime/SQLite) is a projection of the live
//! in-memory task registry: [`SessionStore::project_background_tasks`]
//! reconciles a whole live set in one call, and
//! [`SessionStore::peek_unfinished_background`] /
//! [`SessionStore::consume_unfinished_background`] report + consume whatever a
//! previous process left behind.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
#[cfg(any(feature = "greptime", feature = "sqlite"))]
use std::sync::Arc;
#[cfg(feature = "sqlite")]
use tokio::sync::Mutex;

use anyhow::{Context, Result};

use crate::agent::{Message, SessionEntry};
use std::io::{BufRead, Write};

use crate::config::SessionBackend;
use crate::session::{LoadedSession, Session, UnfinishedTask};

/// Safe, bounded timing information for one metadata listing. This contains
/// only backend names, durations, and row/file counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ListMetaDiagnostics {
    pub backend: &'static str,
    pub facade_lock_wait_ms: u128,
    pub backend_operation_ms: u128,
    pub query_ms: u128,
    pub row_decode_ms: u128,
    pub logical_rows: usize,
    pub connection_lock_wait_ms: u128,
    pub query_iteration_ms: u128,
    pub sidecars_seen: usize,
    pub sidecars_opened: usize,
    pub filesystem_parse_ms: u128,
}

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
/// to be set, so the resolver also consults the OS hostname before falling
/// back to `"unknown"`. The `nonce` disambiguates pid reuse: a simple hash of the
/// boot-time `SystemTime` nanos XORed with the pid — no new dependency,
/// and different processes (or restarts with a reused pid) get different
/// values with overwhelming probability.
static PROCESS_IDENTITY: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Resolve the host name stored in process identity audit metadata.
/// Environment variables are preferred because they are the established
/// container/Windows source; Unix's OS hostname is the next shared source.
fn resolve_hostname<E, F, C>(env: E, os_file: F, command: C) -> String
where
    E: Fn(&str) -> Option<String>,
    F: Fn() -> Option<String>,
    C: Fn() -> Option<String>,
{
    for name in ["HOSTNAME", "COMPUTERNAME"] {
        if let Some(hostname) = env(name).filter(|value| !value.trim().is_empty()) {
            return hostname.trim().to_owned();
        }
    }
    os_file()
        .and_then(|hostname| {
            let hostname = hostname.trim();
            (!hostname.is_empty()).then(|| hostname.to_owned())
        })
        .or_else(|| {
            command().and_then(|hostname| {
                let hostname = hostname.trim();
                (!hostname.is_empty()).then(|| hostname.to_owned())
            })
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The current process's identity string (see [`PROCESS_IDENTITY`]).
pub(crate) fn process_identity() -> &'static str {
    PROCESS_IDENTITY.get_or_init(|| {
        let pid = std::process::id();
        let hostname = resolve_hostname(
            |name| std::env::var(name).ok(),
            || {
                #[cfg(unix)]
                {
                    std::fs::read_to_string("/etc/hostname").ok()
                }
                #[cfg(not(unix))]
                {
                    None
                }
            },
            || {
                #[cfg(not(windows))]
                {
                    std::process::Command::new("hostname")
                        .output()
                        .ok()
                        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
                }
                #[cfg(windows)]
                {
                    None
                }
            },
        );
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

// ----------------------------------------------------------------------
// Shared helpers for the GreptimeDB / SQLite backends
// ----------------------------------------------------------------------
//
// These functions are byte-identical between `session_greptime` and
// `session_sqlite` (each backend historically carried its own copy); they
// live here so both backends `use` the single shared definition. No trait
// (see AGENTS.md) — the `SessionStore` facade dispatches to each backend's
// own methods.

/// Derive a stable workspace identifier from the canonical workspace root
/// path. This mirrors how the JSONL backend uses the root path as a
/// namespace (different workspaces get different on-disk directories).
///
/// `pub` (not `pub(crate)`) because `src/bin/import_jsonl.rs` imports it as
/// `e_agent::session_greptime::derive_workspace_id`; each backend module
/// re-exports it (`pub use`) to preserve that public path.
pub fn derive_workspace_id(root: &Path) -> String {
    root.to_string_lossy().to_string()
}

/// Fingerprint of a workspace id for receipt output: hex SHA-256 of the id,
/// truncated to 16 bytes (32 hex chars). The raw id is a filesystem path on
/// JSONL/SQLite and must never appear in receipts or errors — only this
/// one-way fingerprint does. `workspace_id` is the id as stored by the
/// backend (see [`derive_workspace_id`]).
pub(crate) fn workspace_id_fingerprint(workspace_id: &str) -> String {
    crate::agent::image_sha256(workspace_id.as_bytes())[..32].to_string()
}

/// [`workspace_id_fingerprint`] for a workspace root path.
pub(crate) fn workspace_root_fingerprint(root: &Path) -> String {
    workspace_id_fingerprint(&derive_workspace_id(root))
}

/// Upper bound on session id length, shared by the JSONL name validator
/// ([`crate::session::validate_session_name`]), the receipt payload
/// validation (`src/output_receipt.rs`), and the SQLite schema CHECK
/// constraint (`src/session_sqlite.rs`). Real ids are far shorter
/// (`new_id()` ≈ 23 chars; `web-…` / `sub-…` prefixes add a few); the
/// bound exists so a receipt can never carry an unbounded session string.
pub const MAX_SESSION_ID_LEN: usize = 128;

/// Fingerprint of a backend instance for receipt output: hex SHA-256
/// (truncated to 32 hex chars) over `backend_kind` + NUL + the normalized
/// instance identity — the resolved SQLite database path, the Greptime
/// connection string, or the JSONL workspace root. Legacy receipts carry
/// only this one-way digest (the raw path / connection string never
/// appears in a receipt or an error), and `read_field` rejects a receipt
/// bound to a different backend instance (e.g. another database file or a
/// different GreptimeDB) before querying.
pub(crate) fn backend_instance_fingerprint(backend_kind: &str, instance: &str) -> String {
    let mut identity = String::with_capacity(backend_kind.len() + 1 + instance.len());
    identity.push_str(backend_kind);
    identity.push('\0');
    identity.push_str(instance);
    crate::agent::image_sha256(identity.as_bytes())[..32].to_string()
}

/// Lexically normalize a SQLite database path for backend-instance
/// fingerprinting: absolutize relative paths against the current directory
/// and collapse `.`/`..` components. Never touches the filesystem, so a
/// not-yet-created database file fingerprints identically across
/// processes (and a configured relative path resolves the same way the
/// process actually opens it — against its own cwd).
pub(crate) fn normalize_db_path(path: &str) -> String {
    let path = Path::new(path);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut out = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out.to_string_lossy().into_owned()
}

/// SHA-256 hex of the exact persisted payload bytes (the serialized
/// `SessionEntry`), the entry-hash half of every located key.
pub(crate) fn entry_payload_hash(payload: &str) -> String {
    crate::agent::image_sha256(payload.as_bytes())
}

/// Resolve the SQLite database path for a session store. An explicit
/// `[session] backend = "sqlite"` path wins; `None` (the default backend,
/// or a config that omits `path`) resolves to `<workspace>/.e-agent/
/// sessions.db` — alongside the legacy JSONL `sessions/` directory, so a
/// workspace's session data stays inside the workspace.
pub(crate) fn resolve_sqlite_path(root: &Path, path: Option<&str>) -> String {
    match path {
        Some(p) if !p.trim().is_empty() => p.to_owned(),
        _ => root
            .join(".e-agent")
            .join("sessions.db")
            .to_string_lossy()
            .into_owned(),
    }
}

/// Monotonic real-time microsecond timestamp. Uses wall clock but guarantees
/// strict ordering within a process: if two entries land in the same
/// microsecond (or the clock goes backwards), the later one gets prev+1.
/// Returns microseconds since Unix epoch.
///
/// Greptime stores the value as `TIMESTAMP(9)` (converted via
/// [`us_to_datetime`] for tokio-postgres); SQLite stores it directly as the
/// INTEGER microsecond columns (`event_time_us`, `last_active_at`,
/// `started_at_us`).
pub(crate) fn next_event_time_us() -> i64 {
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

/// Convert microseconds-since-epoch to chrono NaiveDateTime (the type the
/// Greptime backend returns for every timestamp column).
///
/// Input contract: `us` may be negative (pre-1970 timestamps, e.g. -1 µs
/// = 1969-12-31T23:59:59.999999) — `from_timestamp_micros` handles the
/// negative sub-second part natively. Values outside chrono's representable
/// range (roughly year 0000..=9999) panic via the `expect`; the backends
/// only ever store timestamps produced by `next_event_time_us`, which are
/// always in range.
pub(crate) fn us_to_datetime(us: i64) -> chrono::NaiveDateTime {
    chrono::DateTime::from_timestamp_micros(us)
        .expect("event_time µs out of range")
        .naive_utc()
}

/// Convert chrono NaiveDateTime to microseconds-since-epoch for storage
/// (SQLite stores INTEGER microseconds; Greptime stores NaiveDateTime
/// directly and does not need this).
pub(crate) fn datetime_to_us(dt: chrono::NaiveDateTime) -> i64 {
    dt.and_utc().timestamp_micros()
}

/// Classify a SessionEntry for the entry_kind column.
pub(crate) fn entry_kind(entry: &SessionEntry) -> &'static str {
    match entry {
        SessionEntry::Message { .. } => "message",
        SessionEntry::Compaction { .. } => "compaction",
        SessionEntry::Notice { .. } => "notice",
        SessionEntry::BackgroundCompletion { .. } => "background_completion",
        SessionEntry::ForkedFrom { .. } => "forked_from",
        SessionEntry::Error { .. } => "error",
        SessionEntry::GoalUpdated { .. } => "goal_updated",
    }
}

/// Whether an entry flags an error condition. Tool errors and harness
/// errors (persisted `SessionEntry::Error`) both count; the existing
/// Greptime/SQLite `is_error` column is reused for both.
pub(crate) fn is_error(entry: &SessionEntry) -> bool {
    match entry {
        SessionEntry::Message {
            message: Message::Tool { is_error, .. },
        } => *is_error,
        SessionEntry::Error { .. } => true,
        _ => false,
    }
}

/// Group raw DB entries by seq, keeping only the row(s) with the latest
/// `event_time` per seq, and return each winning entry WITH its exact
/// physical key (`seq` + winning `event_time`) AND the exact winning raw
/// payload bytes (the persisted `payload` string, byte-for-byte — the
/// entry-hash input of the located loaders, so the hash always matches
/// what `read_field` re-hashes) — the input of [`SessionStore::load_located`]
/// / the backends' `load_located`.
///
/// - Older event_time rows for the same seq are silently discarded
///   (they are considered overwritten by the newer write).
/// - If the latest event_time has multiple physical rows (same microsecond),
///   their deserialised payloads are compared. All identical → folded
///   (the first raw payload is the winner's bytes); any divergent → `Err`
///   with diagnostic guidance.
/// - Output is sorted by the winning event_time ASC, then seq ASC.
///
/// `event_time_col` names the timestamp column in the inspection SQL of the
/// divergent-duplicates guidance (`event_time` on Greptime,
/// `event_time_us` on SQLite). Exported as `pub(crate)` for testing without
/// a live DB connection.
pub(crate) fn dedup_raw_located(
    raw: &[(i64, chrono::NaiveDateTime, String)],
    session_id: &str,
    workspace_id: &str,
    event_time_col: &str,
) -> Result<Vec<(i64, chrono::NaiveDateTime, SessionEntry, String)>, String> {
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
        let first_entry: SessionEntry = serde_json::from_str(&payloads[0]).map_err(|e| {
            format!(
                "cannot decode session {} (seq {} event_time {}): {e}",
                session_id, seq, max_et
            )
        })?;

        for (i, payload) in payloads.iter().enumerate().skip(1) {
            let other: SessionEntry = serde_json::from_str(payload).map_err(|e| {
                format!(
                    "cannot decode session {} seq {} max_event_time {} (dup {}): {e}",
                    session_id, seq, max_et, i
                )
            })?;
            if other != first_entry {
                return Err(format!(
                    "session '{}' seq {} event_time {} has divergent physical \
                     duplicates; cannot safely load. Stop writers, inspect with SQL:\n\
                     SELECT * FROM session_entries \
                     WHERE workspace_id = '{}' AND session_id = '{}' AND seq = {} \
                     ORDER BY {};\n\
                     Resolve manually (new session or repair) then re-run import.",
                    session_id, seq, max_et, workspace_id, session_id, seq, event_time_col,
                ));
            }
        }

        entries.push((max_et, seq, first_entry, payloads[0].clone()));
    }

    entries.sort_unstable_by_key(|(event_time, seq, _, _)| (*event_time, *seq));
    Ok(entries
        .into_iter()
        .map(|(event_time, seq, entry, payload)| (seq, event_time, entry, payload))
        .collect())
}

/// The seq+winning-event_time view used by the existing load paths (no
/// physical location): dedup by seq (latest event_time wins), sorted by
/// winning event_time ASC then seq ASC. See [`dedup_raw_located`].
pub(crate) fn dedup_raw_entries(
    raw: &[(i64, chrono::NaiveDateTime, String)],
    session_id: &str,
    workspace_id: &str,
    event_time_col: &str,
) -> Result<Vec<(i64, SessionEntry)>, String> {
    dedup_raw_located(raw, session_id, workspace_id, event_time_col).map(|rows| {
        rows.into_iter()
            .map(|(seq, _, entry, _)| (seq, entry))
            .collect()
    })
}

/// Fold the raw `session_entries` rows of `entry_kind = 'background_completion'`
/// (workspace-wide, multiple sessions) into LOGICAL finished-task rows,
/// mirroring the session load path's duplicate semantics
/// ([`dedup_raw_entries`]) but keyed by `(session_id, seq)`:
///
/// 1. Among all physical rows for a given `(session_id, seq)`, only the
///    row(s) with the **latest** `event_time` are retained; older rows are
///    silently discarded (they are superseded).
///
/// 2. If the latest event_time has multiple physical rows (same session,
///    same seq, same max event_time — an idempotent retry at the same
///    microsecond), their payloads are deserialised and compared. If all
///    are identical they are folded into one logical row; if ANY differ,
///    this returns an error naming the session/seq for manual inspection
///    (fail closed — same policy as the load path).
///
/// 3. Output is ordered `event_time DESC, seq DESC` (newest logical row
///    first). With the complete input, callers can apply a limit after this
///    fold to preserve full fail-closed duplicate detection. SQL-bounded
///    callers intentionally limit physical rows before the fold: conflict
///    detection is best-effort within that window, and the result may be
///    shorter than the physical limit.
pub(crate) fn dedup_finished_rows(
    workspace_id: &str,
    raw: &[(String, i64, chrono::NaiveDateTime, String)],
) -> Result<Vec<(String, i64, chrono::NaiveDateTime, String)>, String> {
    use std::cmp::Ordering;
    // Partition by (session_id, seq): keep the max event_time and every
    // payload at that max.
    let mut per_key: std::collections::HashMap<
        (String, i64),
        (chrono::NaiveDateTime, Vec<String>),
    > = std::collections::HashMap::with_capacity(raw.len().min(64));
    for (session_id, seq, event_time, payload) in raw {
        let entry = per_key
            .entry((session_id.clone(), *seq))
            .or_insert((*event_time, Vec::new()));
        match (*event_time).cmp(&entry.0) {
            Ordering::Greater => {
                entry.0 = *event_time;
                entry.1 = vec![payload.clone()];
            }
            Ordering::Equal => entry.1.push(payload.clone()),
            Ordering::Less => {}
        }
    }

    let mut out = Vec::with_capacity(per_key.len());
    for ((session_id, seq), (max_et, payloads)) in per_key {
        let first_entry: SessionEntry = serde_json::from_str(&payloads[0]).map_err(|e| {
            format!(
                "cannot decode finished background task of session {} (seq {} event_time {}): {e}",
                session_id, seq, max_et
            )
        })?;
        for (i, payload) in payloads.iter().enumerate().skip(1) {
            let other: SessionEntry = serde_json::from_str(payload).map_err(|e| {
                format!(
                    "cannot decode finished background task of session {} seq {} max_event_time {} (dup {}): {e}",
                    session_id, seq, max_et, i
                )
            })?;
            if other != first_entry {
                return Err(format!(
                    "session '{}' seq {} event_time {} has divergent physical \
                     duplicates in the finished-task listing; cannot safely load. \
                     Stop writers, inspect with SQL:\n\
                     SELECT * FROM session_entries \
                     WHERE workspace_id = '{}' AND session_id = '{}' AND seq = {} \
                     ORDER BY event_time;\n\
                     Resolve manually (new session or repair) then re-run.",
                    session_id, seq, max_et, workspace_id, session_id, seq,
                ));
            }
        }
        out.push((session_id, seq, max_et, payloads[0].clone()));
    }

    // Newest logical row first: winning event_time DESC, then seq DESC
    // (matching the raw query's tie-break so equal-event_time keys keep a
    // stable, deterministic order).
    out.sort_unstable_by(|a, b| b.2.cmp(&a.2).then_with(|| b.1.cmp(&a.1)));
    Ok(out)
}

/// Format the concurrent-write conflict error shared by the Greptime and
/// SQLite append paths (`db_max >= base_seq` and the overlapping rows
/// diverge from this writer's batch). The `concurrent write conflict`
/// substring is a contract — `main::friendly_failure` matches on it to
/// render the friendly Chinese hint. `writer_hint` is the best-effort
/// "latest metadata writer" read from the sessions audit table (`None` →
/// the plain message without the hint).
pub(crate) fn format_conflict_error(
    session_id: &str,
    db_max: i64,
    base_seq: i64,
    seq: i64,
    writer_hint: Option<&str>,
) -> String {
    match writer_hint {
        Some(writer) => format!(
            "concurrent write conflict on session '{}': database max \
             seq is {db_max} but this writer expected to start at seq \
             {base_seq}; this session is being written concurrently by \
             another client (another e-agent process / TUI / Web \
             window). First divergence at seq {seq}. Stop the other \
             writer or continue in a fresh session, then retry. \
             ; latest metadata writer: {writer}",
            session_id,
        ),
        None => format!(
            "concurrent write conflict on session '{}': database max \
             seq is {db_max} but this writer expected to start at seq \
             {base_seq}; this session is being written concurrently by \
             another client (another e-agent process / TUI / Web \
             window). First divergence at seq {seq}. Stop the other \
             writer or continue in a fresh session, then retry.",
            session_id,
        ),
    }
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
const HEAD_OPEN_SENTINEL: i64 = i64::MAX;

/// Sort one seq-backend page into the canonical DISPLAY order the backends
/// themselves use (winning `event_time` ASC, then `seq` ASC), keeping each
/// row's aligned location with it. A row without a physical location (only
/// possible for JSONL lines or an inherited prefix, never for a stored seq
/// row) sorts last in its existing relative order.
fn sort_page_display_order(
    entries: &mut Vec<SessionEntry>,
    locations: &mut Vec<Option<EntryLocation>>,
) {
    let mut rows: Vec<(Option<EntryLocation>, SessionEntry)> =
        locations.drain(..).zip(entries.drain(..)).collect();
    rows.sort_by_key(|(location, _)| {
        location
            .as_ref()
            .map_or((u8::MAX, i64::MAX, i64::MAX), |location| {
                (
                    u8::MIN,
                    location.key.event_time_us().unwrap_or(i64::MAX),
                    location.key.seq(),
                )
            })
    });
    for (location, entry) in rows {
        locations.push(location);
        entries.push(entry);
    }
}

/// The exact physical key of one persisted `session_entries` row/line, as
/// stored by the owning backend. Pinned (never "latest wins"): a receipt
/// issued against this key reads exactly this physical version, so a
/// same-seq later write can never retarget an old ref (the entry hash check
/// in `read_field` additionally rejects any payload drift).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub enum LocatedKey {
    /// JSONL: the 0-based line ordinal in the append-only `.jsonl` file.
    Jsonl { ordinal: i64 },
    /// SQLite: `seq` + `event_time_us` (the row-level primary key).
    Sqlite { seq: i64, event_time_us: i64 },
    /// GreptimeDB: `seq` + `event_time` (µs since epoch, matching the
    /// `TIMESTAMP(9)` TIME INDEX value this process wrote).
    Greptime { seq: i64, event_time_us: i64 },
}

impl LocatedKey {
    pub fn backend(&self) -> &'static str {
        match self {
            LocatedKey::Jsonl { .. } => "jsonl",
            LocatedKey::Sqlite { .. } => "sqlite",
            LocatedKey::Greptime { .. } => "greptime",
        }
    }

    /// The backend's logical sequence pinned by this key: the JSONL 0-based
    /// line ordinal, or the SQL backends' `seq` column. This is the value a
    /// [`crate::agent::SessionEntry::ForkedFrom`] marker records as `seq`.
    pub fn seq(&self) -> i64 {
        match self {
            LocatedKey::Jsonl { ordinal } => *ordinal,
            LocatedKey::Sqlite { seq, .. } | LocatedKey::Greptime { seq, .. } => *seq,
        }
    }

    /// The row's event time (µs since epoch). `None` on JSONL, which has no
    /// event_time column.
    pub fn event_time_us(&self) -> Option<i64> {
        match self {
            LocatedKey::Jsonl { .. } => None,
            LocatedKey::Sqlite { event_time_us, .. }
            | LocatedKey::Greptime { event_time_us, .. } => Some(*event_time_us),
        }
    }
}

/// The exact physical location of one persisted entry: backend kind
/// fingerprint, backend-instance fingerprint, session id, the backend's
/// pinned key, and the SHA-256 hex of the exact persisted payload bytes
/// (the serialized `SessionEntry` line/row). Aligned 1:1 with the agent
/// history by [`crate::agent::Agent::restore_locations`]; the provider
/// projection (`src/output_receipt.rs`) turns a location into a
/// direct `eout1.<entry-id>.<field-code>` ref, and `read_output` resolves the ref
/// back to this location.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct EntryLocation {
    /// Backend kind code: `"jsonl"` | `"sqlite"` | `"greptime"`.
    pub backend: &'static str,
    /// Hex SHA-256 of the workspace id (NOT the raw path — receipt output
    /// and errors never carry the path).
    pub fingerprint: String,
    /// Hex SHA-256 (32 chars) of the backend-instance identity: backend
    /// kind + normalized instance config (the SQLite database path, the
    /// Greptime connection string, or the JSONL root). Receipts carry only
    /// this hash — never the path/connection string — and `read_field`
    /// rejects a receipt bound to a different instance before querying.
    pub backend_fp: String,
    /// Session id the entry belongs to.
    pub session: String,
    /// The backend's exact physical key.
    pub key: LocatedKey,
    /// SHA-256 hex of the exact persisted payload bytes.
    pub entry_hash: String,
}

/// Minimal physical identity exposed to Web history consumers. Backend
/// fingerprints and payload hashes stay server-side; the UI only needs a
/// stable row key to reconcile bounded pages without inspecting text.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct WebEntryLocation {
    pub backend: &'static str,
    pub key: LocatedKey,
    /// Session that PHYSICALLY stores this row. Backend keys are per-session
    /// (JSONL ordinals restart at 0 in every file), so the same ordinal in an
    /// ancestor and in a fork child is two different rows: the UI reconciles
    /// physical identity, so the owner must be part of it.
    pub session: String,
}

impl From<&EntryLocation> for WebEntryLocation {
    fn from(location: &EntryLocation) -> Self {
        Self {
            backend: location.backend,
            key: location.key.clone(),
            session: location.session.clone(),
        }
    }
}

/// Loaded history paired with its exact physical locations. `locations[i]`
/// is `Some` exactly when the persisted entry at that history position has
/// a located key (all durable backends; legacy/test in-memory entries are
/// `None` — the projection then leaves such fields full instead of emitting
/// an unusable receipt ref).
#[derive(Clone, Debug)]
pub struct LoadedLocated {
    pub entries: Vec<SessionEntry>,
    pub locations: Vec<Option<EntryLocation>>,
    pub legacy: bool,
    /// Number of LEADING `entries` inherited read-only from a zero-copy fork
    /// source ([`SessionStore::load_fork_located`]). Those entries are never
    /// persisted in this session, so they carry no child location; `0` for
    /// every ordinary load (and for the copy-based forks written by older
    /// builds, whose prefix really is stored here).
    pub inherited: usize,
    /// Where this session's OLDER audit rows live when the loaded entries are
    /// a zero-copy fork's materialized context: `(session that physically
    /// stores them, its paging position immediately below `entries[0]`).
    ///
    /// `None` means there is nothing older to page (the rows start at the
    /// owning session's own first row) — or that this load is not a fork
    /// context at all. The position belongs to the NAMED session's physical
    /// space and must never be fed back as a child seq.
    pub next_older: Option<(String, i64)>,
}

impl LoadedLocated {
    /// Cut this loaded history to the first `at` entries and mark them as the
    /// inherited fork prefix: exactly the shape
    /// [`SessionStore::load_fork_located`] returns, with NO child location for
    /// an inherited entry (its row lives in the source session's store, and a
    /// numeric `eout1` ref has no session identity — a child-bound ref could
    /// read the wrong row). Used by the fork producers to seed a child's
    /// in-memory history without writing anything.
    pub fn inherit_prefix(&mut self, at: usize) {
        self.entries.truncate(at);
        self.locations = vec![None; at];
        self.inherited = at;
    }
}

/// One entry of a resolved fork context and where it physically lives.
#[derive(Clone, Debug)]
enum CutOrigin {
    /// A row stored by this session at this exact pinned key.
    Row(String, LocatedKey),
    /// An entry read from `session`'s RESOLVED transcript at this 1-based
    /// index (the prefix of a legacy positional marker).
    Resolved(String, usize),
}

/// The context one zero-copy [`SessionEntry::ForkedFrom`] marker inherits,
/// with the physical origin of every entry (aligned 1:1 with `entries`).
#[derive(Clone, Debug, Default)]
struct InheritedContext {
    entries: Vec<SessionEntry>,
    origins: Vec<CutOrigin>,
    /// Older AUDIT rows below `entries[0]`: the owning session and its paging
    /// position (the compaction that opens the materialized context). `None`
    /// when the context already starts at the owner's own first row.
    older: Option<(String, i64)>,
}

/// Index of the child's OWN [`SessionEntry::ForkedFrom`] marker when this
/// session's stored entries are a zero-copy fork — i.e. the marker is the
/// first stored entry, or the second one right after the child's frozen
/// `GoalUpdated`. Anything else (no marker, or a marker deeper in the log,
/// which is the copy-based fork layout written by older builds that already
/// contains the parent prefix) is not resolved: those entries really are
/// stored here.
pub(crate) fn zero_copy_marker(entries: &[SessionEntry]) -> Option<usize> {
    match entries {
        [SessionEntry::ForkedFrom { .. }, ..] => Some(0),
        [
            SessionEntry::GoalUpdated { .. },
            SessionEntry::ForkedFrom { .. },
            ..,
        ] => Some(1),
        _ => None,
    }
}

/// The DEFAULT fork cut of a session, selected from its physical located head
/// ([`SessionStore::default_fork_cut`]).
#[derive(Clone, Debug)]
pub struct ForkHeadCut {
    /// The session that PHYSICALLY stores the cut row (the session itself, or
    /// an ancestor when the cut is inherited).
    pub owner: String,
    /// That row's exact backend key.
    pub key: LocatedKey,
    /// Exactly the rows the child materializes through the cut: the session's
    /// own head up to its boundary, or the inherited context it reuses.
    pub context: Vec<SessionEntry>,
    /// The 1-based RESOLVED index of the cut when it lies in an inherited
    /// prefix (`None` when the cut is the session's own row).
    pub inherited: Option<usize>,
    /// Older AUDIT rows below `context[0]`: `(owning session, its paging
    /// position)`. `None` = nothing older to page.
    pub older: Option<(String, i64)>,
}

/// One bounded history page with the exact physical identity of every row.
/// The entries and locations remain aligned; cursors retain the existing
/// backend paging contract.
#[derive(Clone, Debug)]
pub struct LocatedHistoryPage {
    pub entries: Vec<SessionEntry>,
    pub locations: Vec<Option<EntryLocation>>,
    pub next_before_seq: Option<i64>,
    /// Session that owns `next_before_seq` when the next older page lives in a
    /// zero-copy fork's ANCESTOR. `None` = this page's own session (the
    /// historical contract).
    pub next_older_session: Option<String>,
}

/// Kind of one [`SessionRef`]. A future kind can be added here without a
/// storage migration: the column carries one JSON value, so only this enum
/// grows.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionRefKind {
    Pr,
    Issue,
}

impl SessionRefKind {
    /// Parse the model/HTTP surface spelling (`"pr"` | `"issue"`).
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "pr" => Ok(Self::Pr),
            "issue" => Ok(Self::Issue),
            other => Err(format!(
                "unknown ref kind `{other}`: expected `pr` or `issue`"
            )),
        }
    }
}

/// One external link a session carries (a pull request or an issue). The URL
/// IS the identity: the same `#14` in two repositories is two different
/// refs, so nothing is reduced to a number.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRef {
    pub kind: SessionRefKind,
    pub url: String,
}

impl SessionRef {
    /// Validate and build one link. `url` must be an absolute `http(s)` URL
    /// without credentials or whitespace; the exact string is preserved (the
    /// link must open, and dedup is by exact URL). Errors are plain strings.
    pub fn new(kind: &str, url: &str) -> Result<Self, String> {
        let kind = SessionRefKind::parse(kind)?;
        if url.is_empty() {
            return Err("ref `url` must not be empty".to_owned());
        }
        if url.chars().any(char::is_whitespace) {
            return Err(format!(
                "`{url}` contains whitespace; a ref URL must be one absolute http(s) URL"
            ));
        }
        // Require an explicit `scheme://` in the ORIGINAL string. `Url::parse`
        // REPAIRS a scheme without an authority (`https:example.org/pull/14`
        // becomes the path `example.org/pull/14` on host "https"), so the
        // checks below would see a host and pass while the raw string — the
        // one stored in `href` and resolved against the UI origin by the
        // browser — has none. Only a string whose scheme is followed by `//`
        // and a non-empty authority is accepted; the exact string is kept.
        let lower = url.to_ascii_lowercase();
        let authority = lower
            .strip_prefix("http://")
            .or_else(|| lower.strip_prefix("https://"))
            .ok_or_else(|| format!("`{url}` must use an explicit http:// or https:// authority"))?;
        // A leading backslash is a malformed authority too: the `url` crate
        // repairs `https://\example.org/x` to host `example.org`, so the raw
        // check must refuse it before the stored string keeps a backslash
        // the browser could resolve against the UI origin.
        if authority.is_empty() || authority.starts_with(&['/', '?', '#', '\\'][..]) {
            return Err(format!(
                "`{url}` must name an authority (host) right after the scheme"
            ));
        }
        let parsed = url::Url::parse(url)
            .map_err(|error| format!("`{url}` is not an absolute URL: {error}"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(format!(
                "`{url}` must use http or https, not `{}`",
                parsed.scheme()
            ));
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(format!("`{url}` must not carry credentials"));
        }
        if parsed.host_str().is_none() {
            return Err(format!("`{url}` must name a host"));
        }
        Ok(Self {
            kind,
            url: url.to_owned(),
        })
    }
}

/// Drop exact-duplicate `(kind, url)` pairs, preserving insertion order: a
/// URL is unique within its kind, never across kinds.
pub fn dedup_refs(refs: &mut Vec<SessionRef>) {
    let mut seen = HashSet::new();
    refs.retain(|link| seen.insert((link.kind, link.url.clone())));
}

/// The wire/storage projection of [`SessionMeta::refs`]: PR URLs then issue
/// URLs, each in insertion order.
pub fn split_refs(refs: &[SessionRef]) -> (Vec<String>, Vec<String>) {
    let mut prs = Vec::new();
    let mut issues = Vec::new();
    for link in refs {
        match link.kind {
            SessionRefKind::Pr => prs.push(link.url.clone()),
            SessionRefKind::Issue => issues.push(link.url.clone()),
        }
    }
    (prs, issues)
}

/// Serialize refs for one JSON column; an empty list stores NULL (the same
/// value a row written before the column existed reads back as).
pub(crate) fn refs_to_json(refs: &[SessionRef]) -> Result<Option<String>, String> {
    if refs.is_empty() {
        return Ok(None);
    }
    serde_json::to_string(refs)
        .map(Some)
        .map_err(|error| format!("cannot encode session refs: {error}"))
}

/// Parse the JSON column back. NULL/blank reads as empty (tolerant, like the
/// other nullable metadata columns); malformed JSON fails loudly instead of
/// silently dropping the session's links.
pub(crate) fn refs_from_json(text: Option<&str>) -> Result<Vec<SessionRef>, String> {
    match text.map(str::trim) {
        None | Some("") => Ok(Vec::new()),
        Some(text) => serde_json::from_str(text)
            .map_err(|error| format!("cannot decode session refs JSON: {error}")),
    }
}

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
    /// Pull-request and issue links, stored as ONE JSON value (a list of
    /// `{"kind","url"}` objects) so a future kind needs no migration. Rows
    /// written before the field existed — the JSONL sidecar lines and the
    /// `refs` column's NULL — read back as empty. `#[serde(default)]` is
    /// REQUIRED: the sidecar tail-read parses each line as `SessionMeta` and
    /// silently skips lines that fail to parse, so a missing default would
    /// hide every pre-existing session from `list_meta`.
    #[serde(default)]
    pub refs: Vec<SessionRef>,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectedBackgroundTask {
    pub id: u64,
    pub label: String,
    pub full_command: Option<String>,
    pub subagent_session_id: Option<String>,
}

#[derive(Clone)]
pub enum SessionStore {
    /// Default file-based JSONL backend. Stateless — delegates to
    /// `session::Session` static methods.
    Jsonl,
    /// GreptimeDB-backed session storage. The client is shared by immutable
    /// references so database operations are not application-serialized.
    #[cfg(feature = "greptime")]
    Greptime {
        /// The connected Greptime session client, session-bound.
        session: Arc<crate::session_greptime::GreptimeSession>,
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

/// One aggregate row of token-usage statistics: totals per
/// (session_id, model, kind) plus the first/last event timestamps
/// (microseconds since epoch, see [`next_event_time_us`]) of that group.
/// Produced by [`SessionStore::usage_summary`] from the `usage_entries`
/// table (Greptime/SQLite backends only; JSONL has no usage table).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageRow {
    pub session_id: String,
    pub model: String,
    /// "regular" | "compact" | "summarizer"
    pub kind: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub first_ts: i64,
    pub last_ts: i64,
}

/// One bounded aggregate row for the public usage dashboard. Enrichment
/// fields remain nullable when no call in the group reported that metric.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageDashboardRow {
    pub session_id: String,
    pub model: String,
    pub kind: String,
    pub bucket_start: i64,
    pub call_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_hit_tokens: Option<u64>,
    pub cache_miss_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cache_hit_reported_calls: u64,
    pub cache_miss_reported_calls: u64,
    pub reasoning_reported_calls: u64,
}

/// The small metadata projection needed by the public usage dashboard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageDashboardMetadata {
    pub session_id: String,
    pub role: Option<String>,
    pub title: Option<String>,
}

/// Cap for background task output previews. The running-task and
/// finished-task APIs use the same character-boundary-safe limit.
pub(crate) const TASK_OUTPUT_LIMIT: usize = 2000;

/// Return a bounded output preview without splitting UTF-8 characters.
pub(crate) fn task_output_preview(output: &str) -> String {
    if output.chars().count() > TASK_OUTPUT_LIMIT {
        output.chars().take(TASK_OUTPUT_LIMIT).collect()
    } else {
        output.to_owned()
    }
}

/// One finished background task, read back from `session_entries`
/// (`entry_kind = 'background_completion'`) for the finished-tasks
/// listing. `finished_at_us` is the row's `event_time` (the durable commit
/// time, µs since epoch); `seq` is the row's sequence, which relates the
/// completion to the surrounding entries of the same session (turn
/// reconstruction is best-effort: ordering across rows is guaranteed by
/// `event_time`/`seq`, but exact equality with a specific assistant entry
/// is not forced).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct FinishedTask {
    pub session_id: String,
    pub seq: i64,
    pub finished_at_us: i64,
    pub id: u64,
    /// Bounded preview of the completed task output.
    pub output: String,
    pub label: Option<String>,
    pub started_at_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    /// "completed" | "failed" | "killed"
    pub status: Option<String>,
    /// "bash" | "delegate"
    pub kind: Option<String>,
    /// Explicit cancellation source, absent for normal/legacy completion.
    pub cancellation_source: Option<crate::agent::CancellationSource>,
}

/// Identifies a session's background-task projection target: the workspace
/// root, the session name the projection belongs to, and the store that owns
/// it. Carried by the delegate tool (`delegate.record_in`) and `BtwContext`
/// to name the parent session that spawned a child (the metadata link);
/// the live task rows themselves come from the runner's projection of the
/// in-memory registry, and are reported as "killed on exit" on resume.
#[derive(Clone)]
pub struct BackgroundRecord {
    pub root: PathBuf,
    pub session: String,
    pub store: SessionStore,
}

/// A logical transcript entry with the namespace that produced it. This is
/// read-only history plumbing; it is intentionally not a second transcript
/// representation.
#[derive(Clone, Debug)]
pub struct HistoryEntry {
    pub workspace_id: String,
    pub session_id: String,
    pub seq: i64,
    pub event_time: Option<chrono::NaiveDateTime>,
    pub entry: SessionEntry,
}

/// One bounded logical-history selection. SQL backends resolve winners and
/// select keys in the database; JSONL retains its existing file fallback.
#[derive(Clone, Debug)]
pub struct HistoryQuery {
    pub workspace_id: Option<String>,
    pub session_id: Option<String>,
    pub query: Option<String>,
    pub after: Option<(String, String, i64)>,
    pub after_event_time: Option<chrono::NaiveDateTime>,
    pub offset: Option<i64>,
    pub exact_seq: Option<i64>,
    pub limit: usize,
}

/// Decode only SQL-selected winner ties. Rows must be ordered so physical
/// ties are adjacent; an unrelated record is deliberately never decoded.
pub(crate) fn decode_history_rows(
    rows: Vec<(String, String, i64, chrono::NaiveDateTime, String)>,
    event_time_col: &str,
) -> Result<Vec<HistoryEntry>, String> {
    let mut out = Vec::new();
    let mut index = 0;
    while index < rows.len() {
        let (workspace_id, session_id, seq, _, _) = &rows[index];
        let start = index;
        while index < rows.len()
            && rows[index].0 == *workspace_id
            && rows[index].1 == *session_id
            && rows[index].2 == *seq
        {
            index += 1;
        }
        let raw = rows[start..index]
            .iter()
            .map(|(_, _, seq, event_time, payload)| (*seq, *event_time, payload.clone()))
            .collect::<Vec<_>>();
        let (_, entry) = dedup_raw_entries(&raw, session_id, workspace_id, event_time_col)?
            .into_iter()
            .next()
            .expect("selected history key has a row");
        out.push(HistoryEntry {
            workspace_id: workspace_id.clone(),
            session_id: session_id.clone(),
            seq: *seq,
            event_time: Some(raw[0].1),
            entry,
        });
    }
    Ok(out)
}

/// What the persisted metadata says about a session's parent link.
enum PersistedParent {
    /// A decoded snapshot records a parent.
    Parent,
    /// Every snapshot line decoded and the newest one records no parent.
    Parentless,
    /// The sidecar exists but cannot prove parentlessness: a record failed
    /// to decode, or it holds no nonblank record at all (an interrupted
    /// first append). Provenance is UNKNOWN.
    Unknown,
}

impl PersistedParent {
    /// Whether provenance must keep `link_ref` off. Only a fully decoded
    /// parentless session does not count as a fork; an undecodable line is
    /// unknown and therefore fails closed.
    fn is_fork(&self) -> bool {
        !matches!(self, Self::Parentless)
    }
}

impl SessionStore {
    pub fn is_jsonl(&self) -> bool {
        matches!(self, SessionStore::Jsonl)
    }
    pub fn supports_history_query(&self) -> bool {
        !matches!(self, SessionStore::Jsonl)
    }

    pub fn history_query_uses_cross_scope_offset(&self) -> bool {
        match self {
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { .. } => true,
            _ => false,
        }
    }

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
                let workspace_id = derive_workspace_id(root);
                let session = crate::session_greptime::GreptimeSession::connect(
                    conn,
                    &workspace_id,
                    session_id,
                )
                .await?;
                Ok(SessionStore::Greptime {
                    session: Arc::new(session),
                    conn: conn.clone(),
                })
            }
            #[cfg(not(feature = "greptime"))]
            SessionBackend::Greptime { .. } => {
                anyhow::bail!("greptime session backend requires the `greptime` cargo feature");
            }
            #[cfg(feature = "sqlite")]
            SessionBackend::Sqlite { path } => {
                let workspace_id = derive_workspace_id(root);
                let path = resolve_sqlite_path(root, path.as_deref());
                let session =
                    crate::session_sqlite::SqliteSession::connect(&path, &workspace_id, session_id)
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
                let workspace_id = derive_workspace_id(root);
                let session = crate::session_greptime::GreptimeSession::connect(
                    conn,
                    &workspace_id,
                    META_STORE_SENTINEL,
                )
                .await?;
                Ok(SessionStore::Greptime {
                    session: Arc::new(session),
                    conn: conn.clone(),
                })
            }
            #[cfg(not(feature = "greptime"))]
            SessionBackend::Greptime { .. } => {
                anyhow::bail!("greptime session backend requires the `greptime` cargo feature");
            }
            #[cfg(feature = "sqlite")]
            SessionBackend::Sqlite { path } => {
                let workspace_id = derive_workspace_id(root);
                let path = resolve_sqlite_path(root, path.as_deref());
                let session = crate::session_sqlite::SqliteSession::connect(
                    &path,
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
            SessionStore::Sqlite { path, .. } => SessionBackend::Sqlite {
                path: Some(path.clone()),
            },
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
                let entries = { session.load().await? };
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

    /// Entry count for one session WITHOUT loading or parsing the whole
    /// transcript (the sessions list calls this for every live session on
    /// every poll — a full `load` per session is a synchronous transcript
    /// parse that blocks the executor).
    ///
    /// JSONL: line-counts `<id>.jsonl` on a blocking thread (zero parse,
    /// [`jsonl_count_transcript_entries`]) so the sync file I/O never
    /// blocks the tokio executor. Greptime/SQLite: the store's in-memory
    /// `next_seq` (= `MAX(seq)+1`, maintained on every append) — no DB
    /// query; the sessions-table `entry_count` snapshot can lag until the
    /// next touch, which the list is tolerant of.
    pub async fn count_entries(&self, root: &Path, session_id: &str) -> Result<i64> {
        match self {
            SessionStore::Jsonl => {
                let root = root.to_owned();
                let session_id = session_id.to_owned();
                tokio::task::spawn_blocking(move || {
                    jsonl_count_transcript_entries(&root, &session_id)
                })
                .await
                .map_err(anyhow::Error::from)?
            }
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => Ok(session.entry_count()),
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => Ok(session.lock().await.entry_count()),
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
                let entries = { session.load_head().await? };
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
    /// The same bounded head contract as [`Self::load_head_page`], retaining
    /// each selected row's physical identity for Web reconciliation.
    ///
    /// On the seq backends a head page that reaches the compaction opening the
    /// head segment is WIDENED to the contiguous seq interval that starts just
    /// after the PREVIOUS compaction (see [`Self::widen_head_page`]), so a Web
    /// client renders the original pre-compaction rows instead of trying to
    /// reconstruct them from the compaction's provider-projection `retained`
    /// tail. JSONL needs none: it loads the whole session, so those rows are
    /// already in the page.
    ///
    /// A zero-copy fork child is resolved first
    /// ([`Self::load_fork_located`]): its physical rows are only the marker
    /// and its own entries, so a physical page would hide the inherited
    /// prefix. The whole resolved transcript is returned as ONE page with no
    /// older cursor — the inherited prefix has no child seq to page by, and
    /// it already starts at position 0 of the source. The leading-marker probe
    /// is BOUNDED (see [`Self::leading_fork_history`]), so an ordinary SQL
    /// session keeps its bounded physical page instead of loading its whole
    /// transcript; JSONL can only be parsed in full, so it resolves directly
    /// (one parse, reused below).
    pub async fn load_head_page_located(
        &self,
        root: &Path,
        name: &str,
        limit: Option<usize>,
    ) -> Result<LocatedHistoryPage> {
        let resolved = self.leading_fork_history(root, name).await?;
        if let Some(resolved) = &resolved
            && resolved.inherited > 0
        {
            // The inherited prefix is served as one page (it has no child
            // seq), but the AUDIT route below it stays available: the cursor
            // is the owning session's position, never a child seq.
            let (cursor, older_session) = match &resolved.next_older {
                // `None` means "this session": an own-session owner is never
                // reported as an ancestor.
                Some((owner, seq)) if owner == name => (Some(*seq), None),
                Some((owner, seq)) => (Some(*seq), Some(owner.clone())),
                None => (None, None),
            };
            return Ok(LocatedHistoryPage {
                entries: resolved.entries.clone(),
                locations: resolved.locations.clone(),
                next_before_seq: cursor,
                next_older_session: older_session,
            });
        }
        match self {
            // JSONL has no seq.  Its cursor is an absolute entry position,
            // including for legacy whole-document sessions whose locations
            // are deliberately absent. The resolved load above already read
            // the file (JSONL can only be parsed in full), so reuse it.
            SessionStore::Jsonl => {
                let loaded = resolved.expect("JSONL always resolves its leading marker");
                let bounded = limit
                    .filter(|&n| n > 0)
                    .filter(|&n| loaded.entries.len() > n)
                    .map(|n| loaded.entries.len() - n);
                // A bounded page's cursor is the ABSOLUTE position of its first
                // RETURNED row (the same value `load_older_located` consumes),
                // never a head-relative index: a head anchored at a compaction
                // starts at an arbitrary ordinal (e.g. 100), so an index of 8
                // would skip everything above it.
                if let Some(start) = bounded {
                    // Absolute position of `entries[0]`: a located row's key
                    // minus its index. Dense JSONL ordinals make every row agree;
                    // a legacy whole-document session has no per-row identity and
                    // starts at position zero.
                    let abs_start = loaded
                        .locations
                        .iter()
                        .enumerate()
                        .find_map(|(index, location)| {
                            location
                                .as_ref()
                                .map(|location| location.key.seq() - index as i64)
                        })
                        .unwrap_or(0);
                    let cursor = loaded.locations[start..]
                        .iter()
                        .find_map(|location| location.as_ref())
                        .map(|location| location.key.seq())
                        .unwrap_or(abs_start + start as i64);
                    return Ok(LocatedHistoryPage {
                        entries: loaded.entries[start..].to_vec(),
                        locations: loaded.locations[start..].to_vec(),
                        next_before_seq: Some(cursor),
                        next_older_session: None,
                    });
                }
                // Whole head: hand out the loaded head's own older route (this
                // session's earlier rows, or a fork ancestor's).
                let (cursor, older_session) = match &loaded.next_older {
                    Some((owner, seq)) if owner == name => (Some(*seq), None),
                    Some((owner, seq)) => (Some(*seq), Some(owner.clone())),
                    None => (None, None),
                };
                Ok(LocatedHistoryPage {
                    entries: loaded.entries,
                    locations: loaded.locations,
                    next_before_seq: cursor,
                    next_older_session: older_session,
                })
            }
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                let (rows, next_before_seq) = session
                    .load_older_located(HEAD_OPEN_SENTINEL, limit)
                    .await?;
                let page = LocatedHistoryPage {
                    entries: rows.iter().map(|(_, entry)| entry.clone()).collect(),
                    locations: rows
                        .into_iter()
                        .map(|(location, _)| Some(location))
                        .collect(),
                    next_before_seq,
                    next_older_session: None,
                };
                self.widen_head_page(root, name, page, limit).await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => {
                let (rows, next_before_seq) = session
                    .lock()
                    .await
                    .load_older_located(HEAD_OPEN_SENTINEL, limit)
                    .await
                    .map_err(anyhow::Error::msg)?;
                let page = LocatedHistoryPage {
                    entries: rows.iter().map(|(_, entry)| entry.clone()).collect(),
                    locations: rows
                        .into_iter()
                        .map(|(location, _)| Some(location))
                        .collect(),
                    next_before_seq,
                    next_older_session: None,
                };
                self.widen_head_page(root, name, page, limit).await
            }
        }
    }

    /// Widen a seq-backend head page into the CONTIGUOUS seq interval that
    /// ends at the newest row and starts right after the row that bounds it:
    /// the PREVIOUS compaction (never crossed — that row opens the next
    /// `load_older` page), or, for a zero-copy fork child, above its
    /// marker/ancestor cut.
    ///
    /// Why an interval instead of a `retained.len()` row count: `retained` is a
    /// provider projection (bounded request copies, synthetic `INTERRUPTED`
    /// results, no `Error`/`GoalUpdated` rows, and orphan tool results the
    /// pairing repair drops), so NO count derived from it names the original
    /// rows — it under- or over-shoots by however many rows repair added or
    /// dropped. The interval is derived from physical seqs alone, so notices,
    /// background completions, tool results and orphan tool results are
    /// included or trimmed by seq, never by kind.
    ///
    /// `limit` is preserved: the compaction and the post-compaction rows are
    /// the NEWEST rows of the interval, so they keep their budget and only the
    /// remainder widens the page — a page truncated below the compaction is
    /// returned untouched. Whether the page still opens the head segment is
    /// decided by its OLDEST PHYSICAL row (minimum seq), never its first
    /// displayed one: display is event-time order, so a rewritten compaction can
    /// display last. The cursor is the MINIMUM included seq, never the
    /// first displayed row, for the same reason: a cursor taken from a
    /// later-displayed but older-seq row would re-return that seq next page.
    async fn widen_head_page(
        &self,
        root: &Path,
        name: &str,
        page: LocatedHistoryPage,
        limit: Option<usize>,
    ) -> Result<LocatedHistoryPage> {
        // Only a page whose OLDEST PHYSICAL row is the compaction that starts the
        // head segment reached that boundary; a `limit`-truncated page below it
        // keeps the compaction + post rows it already returned. The oldest
        // physical row is NOT `entries.first()`: display order is event-time
        // order, so a rewritten compaction (same seq, later winning event time)
        // displays LAST, and the page still opens the head segment.
        let Some((opening, compaction_seq)) = page
            .locations
            .iter()
            .enumerate()
            .filter_map(|(index, location)| {
                location
                    .as_ref()
                    .map(|location| (index, location.key.seq()))
            })
            .min_by_key(|&(_, seq)| seq)
        else {
            return Ok(page);
        };
        let Some(SessionEntry::Compaction { .. }) = page.entries.get(opening) else {
            return Ok(page);
        };
        let budget = match limit {
            Some(limit) => match limit.checked_sub(page.entries.len()) {
                Some(0) | None => return Ok(page),
                Some(budget) => Some(budget),
            },
            // No limit: the whole bounded interval, which still never crosses
            // the previous compaction.
            None => None,
        };
        let interval = self
            .load_older_located(root, name, compaction_seq, budget)
            .await?;
        // A zero-copy fork child's own rows start ABOVE its marker row (which
        // carries the ANCESTOR's cut, not a child seq): the rows at or below it
        // belong to the ancestor, so the interval stops there and the existing
        // fork-aware older handoff keeps owning them.
        let cut = interval
            .entries
            .iter()
            .zip(&interval.locations)
            .find(|(entry, _)| {
                matches!(
                    entry,
                    SessionEntry::ForkedFrom {
                        at: None,
                        seq: Some(_),
                        ..
                    }
                )
            })
            .and_then(|(_, location)| location.as_ref().map(|location| location.key.seq()));
        let mut entries = Vec::with_capacity(interval.entries.len() + page.entries.len());
        let mut locations = Vec::with_capacity(entries.capacity());
        for (entry, location) in interval.entries.into_iter().zip(interval.locations) {
            let Some(seq) = location.as_ref().map(|location| location.key.seq()) else {
                continue;
            };
            // The interval starts AFTER the previous compaction row (it rides
            // with the next older page) and never dips below the fork cut.
            if cut.is_none_or(|cut| seq > cut) && !matches!(entry, SessionEntry::Compaction { .. })
            {
                entries.push(entry);
                locations.push(location);
            }
        }
        if entries.is_empty() {
            return Ok(page);
        }
        entries.extend(page.entries);
        locations.extend(page.locations);
        sort_page_display_order(&mut entries, &mut locations);
        let next_before_seq = locations
            .iter()
            .filter_map(|location| location.as_ref())
            .map(|location| location.key.seq())
            .min()
            .filter(|&seq| seq > 0);
        Ok(LocatedHistoryPage {
            entries,
            locations,
            next_before_seq,
            next_older_session: page.next_older_session,
        })
    }

    /// The same older-page contract as [`Self::load_older`], retaining each
    /// selected row's physical identity.  Database backends use their
    /// bounded segment query directly; JSONL retains its positional cursor.
    pub async fn load_older_located(
        &self,
        root: &Path,
        name: &str,
        before_seq: i64,
        limit: Option<usize>,
    ) -> Result<LocatedHistoryPage> {
        match self {
            SessionStore::Jsonl => {
                if before_seq <= 0 {
                    return Ok(LocatedHistoryPage {
                        entries: Vec::new(),
                        locations: Vec::new(),
                        next_before_seq: None,
                        next_older_session: None,
                    });
                }
                let loaded = Session::load_located(root, name)?;
                let before = (before_seq as usize).min(loaded.entries.len());
                let start = match limit.filter(|&n| n > 0) {
                    Some(n) => before.saturating_sub(n),
                    None => 0,
                };
                Ok(LocatedHistoryPage {
                    entries: loaded.entries[start..before].to_vec(),
                    locations: loaded.locations[start..before].to_vec(),
                    next_before_seq: (start > 0).then_some(start as i64),
                    next_older_session: None,
                })
            }
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                let (rows, next_before_seq) = session.load_older_located(before_seq, limit).await?;
                Ok(LocatedHistoryPage {
                    entries: rows.iter().map(|(_, entry)| entry.clone()).collect(),
                    locations: rows
                        .into_iter()
                        .map(|(location, _)| Some(location))
                        .collect(),
                    next_before_seq,
                    next_older_session: None,
                })
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => {
                let (rows, next_before_seq) = session
                    .lock()
                    .await
                    .load_older_located(before_seq, limit)
                    .await
                    .map_err(anyhow::Error::msg)?;
                Ok(LocatedHistoryPage {
                    entries: rows.iter().map(|(_, entry)| entry.clone()).collect(),
                    locations: rows
                        .into_iter()
                        .map(|(location, _)| Some(location))
                        .collect(),
                    next_before_seq,
                    next_older_session: None,
                })
            }
        }
    }

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
                session.load_older(HEAD_OPEN_SENTINEL, limit).await
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
            SessionStore::Greptime { session, .. } => session.load_older(before_seq, limit).await,
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
            SessionStore::Greptime { session, .. } => session.load_oldest().await,
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
            SessionStore::Greptime { session, .. } => session.load_newer(_after_seq).await,
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
            SessionStore::Greptime { session, .. } => session.head_seq().await,
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
    /// For JSONL there is no seq column; the 0-based ordinal of nonblank JSONL records
    /// is returned so the provenance is still meaningful. For
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
            SessionStore::Greptime { session, .. } => session.load_with_seq().await,
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .load_with_seq()
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// Page identities from this configured store only. JSONL has no catalog
    /// outside the current root; selectors never grant filesystem traversal.
    pub async fn history_session_keys(
        &self,
        root: &Path,
        workspace_id: Option<&str>,
        session_id: Option<&str>,
        after: Option<(&str, &str)>,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        if let Some(id) = session_id {
            crate::session::validate_session_name(id)?;
        }
        match self {
            SessionStore::Jsonl => {
                let workspace = derive_workspace_id(root);
                if workspace_id.is_some_and(|id| id != workspace) {
                    anyhow::bail!("JSONL history is limited to the current workspace");
                }
                let dir = root.join(".e-agent/sessions");
                let files = match std::fs::read_dir(dir) {
                    Ok(files) => files,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(Vec::new());
                    }
                    Err(error) => return Err(error.into()),
                };
                let mut keys = std::collections::BTreeSet::new();
                for file in files {
                    let path = file?.path();
                    if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                        continue;
                    }
                    let Some(name) = path.file_stem().and_then(|x| x.to_str()) else {
                        continue;
                    };
                    if crate::session::validate_session_name(name).is_err()
                        || session_id.is_some_and(|id| id != name)
                        || after.is_some_and(|key| (workspace.as_str(), name) <= key)
                    {
                        continue;
                    }
                    keys.insert((workspace.clone(), name.to_owned()));
                    if keys.len() > limit.min(100) {
                        keys.pop_last();
                    }
                }
                Ok(keys.into_iter().collect())
            }
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                session
                    .history_session_keys(workspace_id, session_id, after, limit)
                    .await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .history_session_keys(workspace_id, session_id, after, limit)
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// Read a selected transcript without changing the runner's binding or
    /// creating a target session. Winner resolution precedes History filtering.
    pub async fn load_history_session(
        &self,
        root: &Path,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<Vec<(i64, SessionEntry)>> {
        crate::session::validate_session_name(session_id)?;
        match self {
            SessionStore::Jsonl => {
                anyhow::ensure!(
                    workspace_id == derive_workspace_id(root),
                    "JSONL history is limited to the current workspace"
                );
                self.load_with_seq(root, session_id).await
            }
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                session
                    .load_history_with_seq(workspace_id, session_id)
                    .await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .load_history_with_seq(workspace_id, session_id)
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// Select logical history rows without loading unrelated payloads on SQL
    /// backends. The JSONL fallback is intentionally left to the tool's
    /// existing file traversal.
    pub async fn query_history(&self, query: &HistoryQuery) -> Result<Vec<HistoryEntry>> {
        match self {
            SessionStore::Jsonl => anyhow::bail!("JSONL history query is unsupported"),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => session.query_history(query).await,
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .query_history(query)
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
            SessionStore::Greptime { session, .. } => session.append(entries).await,
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .append(entries)
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// Append entries to the session log and return the exact physical
    /// located key of every appended entry (durable-append → located-key
    /// ordering: the caller must not emit a receipt ref before this
    /// resolves). JSONL ordinals are the appended records' 0-based nonblank
    /// positions (the file is counted before appending); Greptime/SQLite return
    /// each row's `seq` + `event_time` as pinned at INSERT time.
    ///
    /// For JSONL, `root` and `name` locate the file. For Greptime/SQLite,
    /// the session is already bound so `root`/`name` are unused.
    pub async fn append_located(
        &self,
        root: &Path,
        name: &str,
        entries: &[SessionEntry],
    ) -> Result<Vec<EntryLocation>> {
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        match self {
            SessionStore::Jsonl => Session::append_located(root, name, entries),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => session.append_located(entries).await,
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .append_located(entries)
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// Load the full session history paired with each entry's exact
    /// physical location (`locations[i]` is `Some` for every durable
    /// entry; legacy whole-document JSONL sessions load with `None` — the
    /// projection then leaves those fields full). The located metadata is
    /// aligned 1:1 with [`LoadedLocated::entries`] so the agent can
    /// `restore_history` + `restore_locations` together.
    pub async fn load_located(&self, root: &Path, name: &str) -> Result<LoadedLocated> {
        match self {
            SessionStore::Jsonl => Session::load_located(root, name),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                let rows = session.load_located().await?;
                Ok(LoadedLocated {
                    entries: rows.iter().map(|(_, entry)| entry.clone()).collect(),
                    locations: rows.into_iter().map(|(loc, _)| Some(loc)).collect(),
                    legacy: false,
                    inherited: 0,
                    next_older: None,
                })
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => {
                let rows = session
                    .lock()
                    .await
                    .load_located()
                    .await
                    .map_err(anyhow::Error::msg)?;
                Ok(LoadedLocated {
                    entries: rows.iter().map(|(_, entry)| entry.clone()).collect(),
                    locations: rows.into_iter().map(|(loc, _)| Some(loc)).collect(),
                    legacy: false,
                    inherited: 0,
                    next_older: None,
                })
            }
        }
    }

    /// Marker-aware load: [`Self::load_located`] plus zero-copy fork
    /// resolution.
    ///
    /// A fork child's storage holds ONLY its own entries — the leading
    /// [`SessionEntry::ForkedFrom`] marker (optionally preceded by the
    /// child's frozen `GoalUpdated`). The marker has two generations:
    ///
    /// - **Pinned** (`at: None`): `source` is the session
    ///   that PHYSICALLY stores the cut row, pinned by `seq` (and
    ///   `event_time` on the SQL backends). The context is that owner's rows
    ///   from the last `Compaction` at or before the cut through the cut. When
    ///   the cut precedes every compaction of a marker-first owner, the
    ///   owner's own inherited prefix is resolved instead of its missing
    ///   earlier rows. Nothing after the cut is ever inherited.
    /// - **Legacy positional** (`at: Some(n)`): `source` is the logical
    ///   parent and the first `n` entries of its RESOLVED transcript are
    ///   inherited. Old rows keep loading exactly as written.
    ///
    /// Inherited entries carry `locations[i] = None` (their physical rows
    /// belong to the source session, and a receipt's numeric `eout1` ref has
    /// no session identity — see [`LoadedLocated::inherited`]), so no receipt
    /// is ever issued against the child's store for them.
    ///
    /// Fails closed (naming the source session) when a source is
    /// missing/empty/truncated, when a pinned cut no longer matches the row it
    /// names (a rewritten or reused-`seq` source), when a pinned marker is
    /// missing its `seq`, or when a legacy marker's layout is ambiguous. A
    /// fork of a fork resolves the whole chain with a depth limit.
    pub async fn load_fork_located(&self, root: &Path, name: &str) -> Result<LoadedLocated> {
        self.load_fork_located_depth(root, name, 0).await
    }

    /// [`Self::load_fork_located`] with the ancestry depth threaded through
    /// every recursive hop (a marker chain that names itself must trip the
    /// limit instead of recursing forever).
    async fn load_fork_located_depth(
        &self,
        root: &Path,
        name: &str,
        depth: usize,
    ) -> Result<LoadedLocated> {
        let loaded = self
            .load_located(root, name)
            .await
            .with_context(|| format!("cannot load session {name}"))?;
        let Some(marker) = zero_copy_marker(&loaded.entries) else {
            // An ordinary (or copy-based legacy) log: everything really is
            // stored here.
            return Ok(loaded);
        };
        let context = self
            .fork_marker_context(root, name, &loaded.entries, marker, depth)
            .await?;
        let inherited = context.entries.len();
        let mut entries = context.entries;
        entries.extend(loaded.entries);
        // Inherited entries keep NO child location: their rows live in the
        // source session's store, and a receipt's numeric `eout1` ref carries
        // no session identity, so a child-bound ref could read the wrong row.
        let mut locations = vec![None; inherited];
        locations.extend(loaded.locations);
        Ok(LoadedLocated {
            entries,
            locations,
            // `legacy` describes the PHYSICAL store of the session being
            // loaded (the runner rewrites it), so the child's own flag wins —
            // an inherited legacy whole-document ancestor must not make this
            // session rewrite its marker-only log into a full copy.
            legacy: loaded.legacy,
            inherited,
            next_older: context.older,
        })
    }

    /// The physical identity of the 1-based RESOLVED entry `at` of `name`:
    /// `(session that stores the row, its pinned key)`.
    ///
    /// Fork producers pin exactly this row in a pinned marker (`at: None`), so
    /// the cut can never be retargeted by a later insert/rewrite. When the cut
    /// lies inside a zero-copy inherited prefix the owning session is reached
    /// by descending the marker chain — ownership comes from the selected
    /// physical row, never from a numeric `seq` (a child `seq` and an ancestor
    /// `seq` are different entries).
    ///
    /// Fails closed when the entry has no durable physical identity.
    pub async fn fork_cut_origin(
        &self,
        root: &Path,
        name: &str,
        at: usize,
    ) -> Result<(String, LocatedKey)> {
        self.fork_cut_origin_inner(root, name, at, 0).await
    }

    async fn fork_cut_origin_inner(
        &self,
        root: &Path,
        name: &str,
        at: usize,
        depth: usize,
    ) -> Result<(String, LocatedKey)> {
        anyhow::ensure!(
            depth <= Self::FORK_CHAIN_LIMIT,
            "fork ancestry of session {name} is cyclic or deeper than {} sessions",
            Self::FORK_CHAIN_LIMIT
        );
        anyhow::ensure!(at > 0, "fork point 0 is out of range");
        let store = self.source_store(root, name).await?;
        let own = store
            .load_located(root, name)
            .await
            .with_context(|| format!("cannot load session {name}"))?;
        let Some(marker) = zero_copy_marker(&own.entries) else {
            let key = own
                .locations
                .get(at - 1)
                .and_then(|location| location.as_ref())
                .map(|location| location.key.clone());
            let Some(key) = key else {
                anyhow::bail!(
                    "fork point {at} of session {name} has no durable physical identity: \
                     the entry is not stored by any session"
                );
            };
            return Ok((name.to_owned(), key));
        };
        let context =
            Box::pin(store.fork_marker_context(root, name, &own.entries, marker, depth)).await?;
        if at <= context.entries.len() {
            return match &context.origins[at - 1] {
                CutOrigin::Row(session, key) => Ok((session.clone(), key.clone())),
                CutOrigin::Resolved(session, index) => {
                    Box::pin(self.fork_cut_origin_inner(root, session, *index, depth + 1)).await
                }
            };
        }
        let own_at = at - 1 - context.entries.len();
        let key = own
            .locations
            .get(own_at)
            .and_then(|location| location.as_ref())
            .map(|location| location.key.clone());
        let Some(key) = key else {
            anyhow::bail!(
                "fork point {at} of session {name} has no durable physical identity: \
                 the entry is not stored by any session"
            );
        };
        Ok((name.to_owned(), key))
    }

    /// A store bound to `session` on this backend/root instance. Every method
    /// that touches an OTHER session's rows must connect its own store: the
    /// SQL backends bind the session id at connect time.
    async fn source_store(&self, root: &Path, session: &str) -> Result<Self> {
        Self::connect(&self.backend(), root, session)
            .await
            .with_context(|| format!("cannot open fork source session {session}"))
    }

    /// Resolve what ONE zero-copy marker inherits. `own` is the session's own
    /// stored rows and `marker` the index of its leading `ForkedFrom` entry
    /// (see [`zero_copy_marker`]); `name` is the forking session, used only in
    /// fail-closed messages.
    ///
    /// Recursive (a pinned cut can land inside the owner's own inherited
    /// prefix), so the call edges are boxed and the depth is capped.
    async fn fork_marker_context(
        &self,
        root: &Path,
        name: &str,
        own: &[SessionEntry],
        marker: usize,
        depth: usize,
    ) -> Result<InheritedContext> {
        anyhow::ensure!(
            depth <= Self::FORK_CHAIN_LIMIT,
            "fork ancestry of session {name} is cyclic or deeper than {} sessions",
            Self::FORK_CHAIN_LIMIT
        );
        let SessionEntry::ForkedFrom {
            source,
            at,
            event_time,
            seq,
        } = &own[marker]
        else {
            unreachable!("fork_marker_context is only called on a ForkedFrom marker")
        };
        let (source, event_time, seq) = (source.clone(), *event_time, *seq);
        let Some(at) = *at else {
            // Pinned marker: the absence of `at` selects the owner+key
            // semantics, so a missing `seq` has no row to pin.
            let seq = seq.ok_or_else(|| {
                anyhow::anyhow!(
                    "fork session {name} is unusable: its ForkedFrom marker names source \
                     {source} without a pinned seq; re-record the fork"
                )
            })?;
            return Box::pin(self.fork_owner_context(
                root,
                name,
                &source,
                seq,
                event_time,
                depth + 1,
            ))
            .await;
        };
        if at == 0 {
            // Legacy: nothing inherited, the child is self-contained.
            return Ok(InheritedContext::default());
        }
        let store = self.source_store(root, &source).await?;
        let resolved = Box::pin(store.load_fork_located_depth(root, &source, depth + 1))
            .await
            .with_context(|| format!("cannot load fork source session {source}"))?;
        // Commit `0e32549` wrote the copy-based layout `[ForkedFrom] +
        // prefix` (marker FIRST). That layout is byte-identical to a genuine
        // zero-copy child whose own first messages happen to repeat the parent
        // prefix, so content equality alone cannot decide. Provenance can: the
        // old layout never froze a goal before the marker and never wrote
        // `event_time`, so a marker placed after the child's `GoalUpdated` —
        // or one carrying `event_time` — is zero-copy. Everything else with
        // the source prefix stored right after a leading marker is AMBIGUOUS
        // and fails closed rather than guessing: "legacy" would silently drop
        // the parent turn, "zero-copy" would duplicate it.
        let stored_prefix = marker + 1..marker + 1 + at;
        if marker == 0
            && event_time.is_none()
            && resolved.entries.len() >= at
            && own.len() >= marker + 1 + at
            && own[stored_prefix] == resolved.entries[..at]
        {
            anyhow::bail!(
                "fork session {name} is ambiguous: its ForkedFrom marker (source {source}, \
                 at {at}) is followed by {at} stored entries identical to the source \
                 prefix, which is exactly the layout of the legacy marker-first fork copy. \
                 Refusing to guess between dropping the inherited prefix and duplicating \
                 it; re-record the fork with a marker carrying event_time (or without the \
                 stored prefix) to disambiguate."
            );
        }
        anyhow::ensure!(
            !resolved.entries.is_empty(),
            "fork source session {source} is missing or empty; session {name} was \
             forked at entry {at} and cannot be loaded without it"
        );
        anyhow::ensure!(
            resolved.entries.len() >= at,
            "fork source session {source} has only {} entries but session {name} was \
             forked at entry {at}",
            resolved.entries.len()
        );
        // Pin the cut: the loaded source must still carry the exact
        // `seq`/`event_time` the marker recorded (a rewritten source — e.g. a
        // compaction that replaced entries — fails closed instead of silently
        // inheriting a different prefix). A source that is itself a fork child
        // may sit inside an already-inherited region; such entries carry no
        // physical location of their own and are accepted positionally, as are
        // legacy whole-document sources.
        if let Some(end) = resolved.locations.get(at - 1).and_then(|loc| loc.as_ref()) {
            if let Some(expected) = seq {
                anyhow::ensure!(
                    end.key.seq() == expected,
                    "fork source session {source} no longer matches the marker of \
                     session {name}: entry {at} moved to seq {}, the marker pinned \
                     {expected}",
                    end.key.seq()
                );
            }
            if let Some(expected) = event_time {
                anyhow::ensure!(
                    end.key.event_time_us() == Some(expected),
                    "fork source session {source} no longer matches the marker of \
                     session {name}: entry {at} moved to event_time {:?}, the marker \
                     pinned {expected}",
                    end.key.event_time_us()
                );
            }
        }
        Ok(InheritedContext {
            entries: resolved.entries[..at].to_vec(),
            // Each inherited entry is the source's RESOLVED entry at the same
            // 1-based index (legacy markers are positional by definition).
            origins: (1..=at)
                .map(|index| CutOrigin::Resolved(source.clone(), index))
                .collect(),
            // A legacy positional prefix has no physical cursor of its own:
            // paging further back stays a documented legacy boundary.
            older: None,
        })
    }

    /// The context a PINNED marker inherits: the owner's physical rows from
    /// the last [`SessionEntry::Compaction`] at or before the cut through the
    /// cut itself.
    ///
    /// The owner's physical head is read first and older compaction segments
    /// are paged in only until the segment holding the cut is reached, so a
    /// cut far behind the owner's newest compaction costs one segment, not the
    /// whole transcript. Rows after the cut are dropped: the child inherits
    /// exactly the parent's prefix, never later appends.
    #[allow(clippy::too_many_arguments)]
    async fn fork_owner_context(
        &self,
        root: &Path,
        name: &str,
        owner: &str,
        seq: i64,
        event_time: Option<i64>,
        depth: usize,
    ) -> Result<InheritedContext> {
        anyhow::ensure!(
            depth <= Self::FORK_CHAIN_LIMIT,
            "fork ancestry of session {name} is cyclic or deeper than {} sessions",
            Self::FORK_CHAIN_LIMIT
        );
        let store = self.source_store(root, owner).await?;
        let mut page = store
            .load_older_located(root, owner, HEAD_OPEN_SENTINEL, None)
            .await
            .with_context(|| format!("cannot load fork source session {owner}"))?;
        let cut = loop {
            if let Some(index) = page.locations.iter().position(|location| {
                location
                    .as_ref()
                    .is_some_and(|location| location.key.seq() == seq)
            }) {
                break index;
            }
            // The cut sits behind this segment: page the immediately older
            // compaction segment in (the cursor is the compaction opening the
            // segment just read; `None` means its start is the session's own).
            let Some(before) = page.next_before_seq else {
                anyhow::bail!(
                    "fork source session {owner} no longer contains the entry pinned by \
                     the marker of session {name} (seq {seq}); the forked history cannot \
                     be loaded"
                );
            };
            page = store
                .load_older_located(root, owner, before, None)
                .await
                .with_context(|| format!("cannot load fork source session {owner}"))?;
        };
        let key = page.locations[cut]
            .as_ref()
            .expect("every row of a located page carries its key")
            .key
            .clone();
        anyhow::ensure!(
            key.event_time_us() == event_time,
            "fork source session {owner} no longer matches the marker of session {name}: \
             the pinned entry (seq {seq}) moved to event_time {:?}, the marker pinned \
             {event_time:?}",
            key.event_time_us()
        );
        let row_origin = |index: usize| {
            CutOrigin::Row(
                owner.to_owned(),
                page.locations[index]
                    .as_ref()
                    .expect("every row of a located page carries its key")
                    .key
                    .clone(),
            )
        };
        // The last compaction AT OR BEFORE the cut opens the context (the
        // owner's latest compaction overall would be a different, later one).
        // Earlier rows are already summarized there and are not needed.
        if let Some(start) = (0..=cut)
            .rev()
            .find(|&index| matches!(page.entries[index], SessionEntry::Compaction { .. }))
        {
            // The context opens at that compaction, so the owner's rows BELOW
            // it are the audit-only older history: keep the route (never a
            // child seq) instead of materializing them into the model context.
            let older = Some((
                owner.to_owned(),
                page.locations[start]
                    .as_ref()
                    .expect("every row of a located page carries its key")
                    .key
                    .seq(),
            ));
            return Ok(InheritedContext {
                entries: page.entries[start..=cut].to_vec(),
                origins: (start..=cut).map(row_origin).collect(),
                older,
            });
        }
        // No compaction at or before the cut: the cut lies in the owner's own
        // oldest rows. When that oldest segment starts with the owner's own
        // zero-copy marker, the owner inherits ITS prefix — resolve it instead
        // of pretending the owner's first stored row is the session start.
        let mut entries = Vec::new();
        let mut origins = Vec::new();
        let mut older = None;
        if let Some(marker) = zero_copy_marker(&page.entries).filter(|&marker| marker <= cut) {
            let prefix =
                Box::pin(store.fork_marker_context(root, owner, &page.entries, marker, depth + 1))
                    .await?;
            entries.extend(prefix.entries);
            origins.extend(prefix.origins);
            older = prefix.older;
        }
        entries.extend(page.entries[..=cut].iter().cloned());
        origins.extend((0..=cut).map(row_origin));
        // The owner's own first row opens this part, but a prepended inherited
        // prefix may still have older rows below ITS start: keep that route so
        // audit paging continues in the ancestor.
        Ok(InheritedContext {
            entries,
            origins,
            older,
        })
    }

    /// The session's OWN physical head: the last `Compaction` and everything
    /// after it, with each row's key. Older rows stay on demand through
    /// [`Self::load_older_located`] (the returned `next_older` route).
    ///
    /// This is the MODEL-CONTEXT view of a session: [`crate::agent::Agent`]
    /// projects exactly `[last compaction, ∞)`, so initializing from the head
    /// is equivalent to loading the whole transcript while a paged backend
    /// reads one segment instead of everything. Legacy whole-document sessions
    /// (`.json`, rewritten in place on resume) are returned in full, and JSONL
    /// is always parsed in full — only what is RETURNED is bounded there.
    pub async fn load_head_located(&self, root: &Path, name: &str) -> Result<LoadedLocated> {
        match self {
            SessionStore::Jsonl => Ok(Self::jsonl_head(Session::load_located(root, name)?, name)),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                let (rows, cursor) = session.load_older_located(HEAD_OPEN_SENTINEL, None).await?;
                Ok(LoadedLocated {
                    entries: rows.iter().map(|(_, entry)| entry.clone()).collect(),
                    locations: rows
                        .into_iter()
                        .map(|(location, _)| Some(location))
                        .collect(),
                    legacy: false,
                    inherited: 0,
                    next_older: cursor.map(|seq| (name.to_owned(), seq)),
                })
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => {
                let (rows, cursor) = session
                    .lock()
                    .await
                    .load_older_located(HEAD_OPEN_SENTINEL, None)
                    .await
                    .map_err(anyhow::Error::msg)?;
                Ok(LoadedLocated {
                    entries: rows.iter().map(|(_, entry)| entry.clone()).collect(),
                    locations: rows
                        .into_iter()
                        .map(|(location, _)| Some(location))
                        .collect(),
                    legacy: false,
                    inherited: 0,
                    next_older: cursor.map(|seq| (name.to_owned(), seq)),
                })
            }
        }
    }

    /// Slice a fully decoded JSONL session to its physical head: the last
    /// `Compaction` and everything after it. The caller must ask any
    /// whole-log question (fork provenance) BEFORE this slice, because the
    /// rows below the cut are dropped here.
    fn jsonl_head(loaded: LoadedLocated, name: &str) -> LoadedLocated {
        if loaded.legacy {
            return loaded;
        }
        let start = loaded
            .entries
            .iter()
            .rposition(|entry| matches!(entry, SessionEntry::Compaction { .. }))
            .unwrap_or(0);
        LoadedLocated {
            entries: loaded.entries[start..].to_vec(),
            locations: loaded.locations[start..].to_vec(),
            legacy: false,
            inherited: 0,
            next_older: (start > 0).then(|| (name.to_owned(), start as i64)),
        }
    }

    /// HEAD-oriented marker-aware load: the model context this session
    /// initializes from, without materializing the whole transcript.
    ///
    /// - No leading fork marker: [`Self::load_head_located`] — the session's
    ///   own head, with older rows available on demand.
    /// - Leading fork marker (zero-copy child) that already compacted on its
    ///   own: that head IS the context; the ancestry below it is not needed.
    /// - Leading fork marker and no own compaction: the marker's inherited
    ///   context is resolved (see [`Self::fork_marker_context`]) and
    ///   prepended to the child's own rows, exactly like
    ///   [`Self::load_fork_located`], and the context's older audit rows stay
    ///   behind [`LoadedLocated::next_older`].
    ///
    /// Use [`Self::load_fork_located`] when the FULL resolved transcript is
    /// required (fork-boundary scans, explicit `--at` resolution).
    pub async fn load_fork_head_located(&self, root: &Path, name: &str) -> Result<LoadedLocated> {
        let head = self.load_head_located(root, name).await?;
        self.resolve_fork_head(root, name, head).await
    }

    /// The factory's reopen load: [`Self::load_fork_head_located`] PLUS the
    /// persisted session's fork provenance, from ONE JSONL decode.
    ///
    /// JSONL has no kind index, so the head load already decodes every entry
    /// before slicing at the last compaction. Answering provenance from those
    /// entries (before the slice) keeps a large ordinary transcript to one
    /// sequential pass instead of scanning it a second time. The SQL backends
    /// keep their bounded `LIMIT 1` marker query.
    ///
    /// Returns `(head, is_fork)`. The metadata parent is still part of the
    /// answer: a child whose own log carries no marker is a fork when its
    /// sidecar/row records one, and an unreadable, undecodable, or
    /// never-written sidecar is never "no parent".
    pub(crate) async fn load_fork_head_and_provenance(
        &self,
        root: &Path,
        name: &str,
    ) -> Result<(LoadedLocated, bool)> {
        match self {
            SessionStore::Jsonl => {
                // Sidecar first: an unreadable parent link fails this reopen,
                // and an undecodable record or an empty sidecar (an
                // interrupted first append) is unknown.
                let persisted_parent = self.metadata_parent(root, name).await?;
                let loaded = Session::load_located(root, name)?;
                let has_marker = loaded
                    .entries
                    .iter()
                    .any(|entry| matches!(entry, SessionEntry::ForkedFrom { .. }));
                let is_fork = persisted_parent.is_fork() || has_marker;
                let head = Self::jsonl_head(loaded, name);
                Ok((self.resolve_fork_head(root, name, head).await?, is_fork))
            }
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { .. } => {
                self.load_fork_head_and_query_provenance(root, name).await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { .. } => {
                self.load_fork_head_and_query_provenance(root, name).await
            }
        }
    }

    /// Bounded-backend half of [`Self::load_fork_head_and_provenance`].
    async fn load_fork_head_and_query_provenance(
        &self,
        root: &Path,
        name: &str,
    ) -> Result<(LoadedLocated, bool)> {
        let head = self.load_fork_head_located(root, name).await?;
        let is_fork = self.has_fork_provenance(root, name).await?;
        Ok((head, is_fork))
    }

    /// Prepend a zero-copy child's inherited context to its own physical
    /// head. Shared by [`Self::load_fork_head_located`] and the factory's
    /// one-decode reopen load.
    async fn resolve_fork_head(
        &self,
        root: &Path,
        name: &str,
        head: LoadedLocated,
    ) -> Result<LoadedLocated> {
        if head.legacy {
            // A legacy whole-document session has no physical head to page.
            return Ok(head);
        }
        let Some(marker) = zero_copy_marker(&head.entries) else {
            return Ok(head);
        };
        let context = self
            .fork_marker_context(root, name, &head.entries, marker, 0)
            .await?;
        let inherited = context.entries.len();
        let mut entries = context.entries;
        entries.extend(head.entries);
        let mut locations = vec![None; inherited];
        locations.extend(head.locations);
        Ok(LoadedLocated {
            entries,
            locations,
            legacy: false,
            inherited,
            next_older: context.older,
        })
    }

    /// The DEFAULT fork cut of `name`, selected from a physical located HEAD
    /// instead of a full resolved transcript.
    ///
    /// The cut is the newest completed-turn boundary this session can see: its
    /// own last boundary when it has one, otherwise the cut its own zero-copy
    /// marker inherits (descending only because the selected prefix needs the
    /// ancestry). [`ForkHeadCut::context`] is exactly the rows the child will
    /// materialize, so a producer can prove the boundary without loading the
    /// rest of the transcript.
    pub async fn default_fork_cut(&self, root: &Path, name: &str) -> Result<ForkHeadCut> {
        let head = self.load_head_located(root, name).await?;
        if let Some(index) = head
            .entries
            .iter()
            .rposition(crate::agent::is_turn_boundary)
        {
            let key = head.locations[index]
                .as_ref()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "fork cut of session {name} has no durable physical identity: \
                         the boundary entry is not stored by any session"
                    )
                })?
                .key
                .clone();
            // A marker-first head (no own compaction yet) reaches its own
            // completed turn only through the prefix it inherits: the child
            // materializes BOTH, so resolve that prefix here — otherwise a
            // fresh seed would disagree with what reloading the child
            // resolves.
            let (mut entries, inherited, older) =
                match zero_copy_marker(&head.entries).filter(|&marker| marker <= index) {
                    Some(marker) => {
                        let prefix = self
                            .fork_marker_context(root, name, &head.entries, marker, 0)
                            .await?;
                        let inherited = prefix.entries.len();
                        let mut entries = prefix.entries;
                        entries.extend(head.entries.iter().cloned());
                        (entries, Some(inherited), prefix.older)
                    }
                    None => (head.entries.clone(), None, head.next_older.clone()),
                };
            let keep = inherited.unwrap_or(0) + index + 1;
            entries.truncate(keep);
            return Ok(ForkHeadCut {
                owner: name.to_owned(),
                key,
                context: entries,
                inherited,
                // A prepended prefix moves the context start into the
                // ancestor, so its own older route wins.
                older,
            });
        }
        // No boundary of its own: a zero-copy child inherits its marker's cut.
        let Some(marker) = zero_copy_marker(&head.entries) else {
            return Err(anyhow::anyhow!("no completed turn in session"));
        };
        let context = self
            .fork_marker_context(root, name, &head.entries, marker, 0)
            .await?;
        let SessionEntry::ForkedFrom {
            source,
            at,
            event_time,
            seq,
        } = &head.entries[marker]
        else {
            unreachable!("zero_copy_marker only matches ForkedFrom")
        };
        let (owner, key) = match at {
            // Legacy positional marker: resolving it is allowed to page/load
            // the ancestor's resolved transcript.
            Some(at) => self.fork_cut_origin(root, name, *at).await?,
            None => {
                let seq = seq.ok_or_else(|| {
                    anyhow::anyhow!(
                        "fork session {name} is unusable: its ForkedFrom marker names \
                         source {source} without a pinned seq; re-record the fork"
                    )
                })?;
                (source.clone(), self.marker_key(seq, *event_time))
            }
        };
        anyhow::ensure!(
            !context.entries.is_empty(),
            "fork source session {owner} is missing or empty; session {name} was \
             forked at that cut and cannot be forked again without it"
        );
        Ok(ForkHeadCut {
            owner,
            key,
            inherited: Some(context.entries.len()),
            context: context.entries,
            older: context.older,
        })
    }

    /// The backend key a marker's `seq`/`event_time` pair names.
    fn marker_key(&self, seq: i64, event_time: Option<i64>) -> LocatedKey {
        match self.backend() {
            SessionBackend::Jsonl => LocatedKey::Jsonl { ordinal: seq },
            #[cfg(feature = "sqlite")]
            SessionBackend::Sqlite { .. } => LocatedKey::Sqlite {
                seq,
                event_time_us: event_time.unwrap_or_default(),
            },
            #[cfg(feature = "greptime")]
            SessionBackend::Greptime { .. } => LocatedKey::Greptime {
                seq,
                event_time_us: event_time.unwrap_or_default(),
            },
        }
    }

    /// The goal that applies AT a fork cut: the newest `GoalUpdated` whose row
    /// is at or below `key` — never one updated after the cut.
    ///
    /// A `GoalUpdated` is not embedded in a compaction summary, and the
    /// materialized context starts at the last compaction at or before the
    /// cut, so absence inside that context means "look earlier", not "no
    /// goal": the owner's own rows are queried first, then the owner's own
    /// inherited prefix (when the owner is itself a zero-copy child) is
    /// followed with its own cut as the new bound.
    ///
    /// `None` = no goal update at/before the cut; `Some(None)` = the newest
    /// one is a clear tombstone.
    pub async fn fork_goal_at_cut(
        &self,
        root: &Path,
        owner: &str,
        key: &LocatedKey,
    ) -> Result<Option<Option<crate::agent::GoalSnapshot>>> {
        self.fork_goal_before(root, owner, key.seq()).await
    }

    /// The newest goal update anywhere in this session's ancestry (the LIVE
    /// goal a delegate fork freezes, independent of its cut).
    pub async fn latest_goal(
        &self,
        root: &Path,
        name: &str,
    ) -> Result<Option<Option<crate::agent::GoalSnapshot>>> {
        self.fork_goal_before(root, name, i64::MAX).await
    }

    async fn fork_goal_before(
        &self,
        root: &Path,
        owner: &str,
        cutoff: i64,
    ) -> Result<Option<Option<crate::agent::GoalSnapshot>>> {
        let mut session = owner.to_owned();
        let mut cutoff = cutoff;
        for _ in 0..Self::FORK_CHAIN_LIMIT {
            let store = self.source_store(root, &session).await?;
            let found = match &store {
                SessionStore::Jsonl => Session::load(root, &session)?
                    .entries
                    .iter()
                    .enumerate()
                    // JSONL has no seq column: the marker's `seq` is the
                    // 0-based line ordinal, so the cutoff compares directly.
                    .filter(|(index, _)| (*index as i64) <= cutoff)
                    .filter_map(|(_, entry)| match entry {
                        SessionEntry::GoalUpdated { goal } => Some(goal.clone()),
                        _ => None,
                    })
                    // Newest first from the back: the goal that applies AT the
                    // cut, never a later update.
                    .next_back(),
                #[cfg(feature = "greptime")]
                SessionStore::Greptime { session, .. } => session
                    .latest_goal_before(cutoff)
                    .await?
                    .and_then(|entry| match entry {
                        SessionEntry::GoalUpdated { goal } => Some(goal),
                        _ => None,
                    }),
                #[cfg(feature = "sqlite")]
                SessionStore::Sqlite { session, .. } => session
                    .lock()
                    .await
                    .latest_goal_before(cutoff)
                    .await
                    .map_err(anyhow::Error::msg)?
                    .and_then(|entry| match entry {
                        SessionEntry::GoalUpdated { goal } => Some(goal),
                        _ => None,
                    }),
            };
            if let Some(goal) = found {
                return Ok(Some(goal));
            }
            // Not in this session's own rows: it may live in the prefix this
            // session itself inherits, bounded by its OWN cut.
            let Some(marker) = store.leading_fork_marker(root, &session).await? else {
                return Ok(None);
            };
            let SessionEntry::ForkedFrom {
                source,
                at,
                event_time,
                seq,
            } = marker
            else {
                unreachable!("leading_fork_marker only returns ForkedFrom")
            };
            match at {
                Some(at) => {
                    // Legacy positional prefix: resolve it and fold positionally.
                    let resolved = Box::pin(store.load_fork_located_depth(root, &source, 1))
                        .await
                        .with_context(|| format!("cannot load fork source session {source}"))?;
                    anyhow::ensure!(
                        resolved.entries.len() >= at,
                        "fork source session {source} has only {} entries but session \
                         {session} was forked at entry {at}",
                        resolved.entries.len()
                    );
                    let (goal, cleared) = crate::agent::Agent::fold_goal(&resolved.entries[..at]);
                    return Ok((goal.is_some() || cleared).then_some(goal));
                }
                None => {
                    let seq = seq.ok_or_else(|| {
                        anyhow::anyhow!(
                            "fork session {session} is unusable: its ForkedFrom marker \
                             names source {source} without a pinned seq; re-record the fork"
                        )
                    })?;
                    session = source;
                    cutoff = seq;
                    let _ = event_time;
                }
            }
        }
        anyhow::bail!(
            "fork ancestry of session {owner} is cyclic or deeper than {} sessions",
            Self::FORK_CHAIN_LIMIT
        )
    }

    /// One older AUDIT page of a zero-copy fork's inherited history.
    ///
    /// `owner` is the session named by the head page's
    /// [`LocatedHistoryPage::next_older_session`]; `before_seq` is its physical
    /// position (never a child seq). Rows at or above the fork's pinned cut are
    /// never returned, and when the owner's own oldest rows are reached the
    /// page continues in the ancestor the owner's own marker names (a pinned
    /// marker only: a legacy positional prefix has no physical cursor).
    pub async fn load_fork_older_located(
        &self,
        root: &Path,
        owner: &str,
        before_seq: i64,
        limit: Option<usize>,
    ) -> Result<LocatedHistoryPage> {
        let store = self.source_store(root, owner).await?;
        let mut page = store
            .load_older_located(root, owner, before_seq, limit)
            .await?;
        if page.next_before_seq.is_none()
            && let Some(SessionEntry::ForkedFrom {
                source,
                at: None,
                seq: Some(seq),
                ..
            }) = store.leading_fork_marker(root, owner).await?
        {
            // The owner's own rows are exhausted and it is itself a zero-copy
            // child: continue in the ancestor that physically stores the rows
            // below the owner's cut. The ancestor's pinned cut row was NOT
            // materialized (a compaction summarized it), so the cursor is
            // INCLUSIVE of it: `seq + 1` is the exclusive bound one row later.
            // Rows after the cut are still excluded.
            page.next_before_seq = Some(seq.saturating_add(1));
            page.next_older_session = Some(source);
        } else if page.next_before_seq.is_some() {
            // Another page of THIS owner follows: the cursor still belongs to
            // the owner, never to the child that requested it.
            page.next_older_session = Some(owner.to_owned());
        }
        Ok(page)
    }

    /// The first `bound` stored entries through the backend's own bounded
    /// reader: JSONL stops reading after `bound` lines, the SQL backends
    /// LIMIT their payload query. Every LEADING-marker probe goes through
    /// this one path; general fork provenance asks the marker query below
    /// instead, because a legacy copy marker can sit behind the whole log.
    async fn load_first_entries(
        &self,
        root: &Path,
        name: &str,
        bound: usize,
    ) -> Result<Vec<SessionEntry>> {
        match self {
            SessionStore::Jsonl => Session::load_first(root, name, bound),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => session.load_first(bound).await,
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .load_first(bound)
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// The session's own leading zero-copy marker, if it has one. Bounded: only
    /// the FIRST few logical rows are read.
    async fn leading_fork_marker(&self, root: &Path, name: &str) -> Result<Option<SessionEntry>> {
        let entries = self.load_first_entries(root, name, 3).await?;
        Ok(zero_copy_marker(&entries).map(|marker| entries[marker].clone()))
    }

    /// Whether the PERSISTED session is a fork.
    ///
    /// 1. A persisted parent link (the metadata row a delegate / btw / fork
    ///    child gets) decides it with no transcript read at all.
    /// 2. Otherwise ask the stored log through the store's own marker query:
    ///    a `ForkedFrom` at ANY depth — leading (zero-copy), after the frozen
    ///    goal, or behind the whole copied prefix (the copy-based writer this
    ///    repo shipped before `1edc53d`). SQL fetches ONE matching row and no
    ///    payload; JSONL streams until the first marker and never parses a
    ///    legacy `.json` document (it stores messages only).
    ///
    /// There is deliberately no fixed head bound: a constant would silently
    /// answer "not a fork" for a marker it never looked at, which is exactly
    /// how a reopened copy fork regained `link_ref`.
    ///
    /// The factory's reopen path does NOT call this on JSONL: its head load
    /// already decodes the transcript, so it uses
    /// [`Self::load_fork_head_and_provenance`] and a long session is scanned
    /// once instead of twice.
    pub(crate) async fn has_fork_provenance(&self, root: &Path, name: &str) -> Result<bool> {
        if self.metadata_parent(root, name).await?.is_fork() {
            return Ok(true);
        }
        self.transcript_has_fork_marker(root, name).await
    }

    /// The session's persisted parent-link evidence. One narrow read: the
    /// newest sidecar snapshot on JSONL (plus whether the sidecar can prove
    /// parentlessness at all), a single-row/single-column SELECT on the SQL
    /// backends.
    async fn metadata_parent(&self, root: &Path, name: &str) -> Result<PersistedParent> {
        match self {
            SessionStore::Jsonl => {
                let (latest, unknown) = jsonl_read_meta_snapshot_and_unknown(root, name)?;
                let parent = latest.and_then(|meta| meta.parent_session_id);
                Ok(match (parent, unknown) {
                    (Some(_), _) => PersistedParent::Parent,
                    (None, true) => PersistedParent::Unknown,
                    (None, false) => PersistedParent::Parentless,
                })
            }
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                Ok(match session.parent_session_id().await? {
                    Some(_) => PersistedParent::Parent,
                    None => PersistedParent::Parentless,
                })
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => Ok(
                match session
                    .lock()
                    .await
                    .parent_session_id()
                    .await
                    .map_err(anyhow::Error::msg)?
                {
                    Some(_) => PersistedParent::Parent,
                    None => PersistedParent::Parentless,
                },
            ),
        }
    }

    /// The store's own marker query: does the transcript contain a
    /// `ForkedFrom` entry anywhere? The SQL backends match `entry_kind` and
    /// `LIMIT 1` without selecting the payload, so the I/O is bounded by the
    /// ANSWER (one row), not by a logical head count.
    async fn transcript_has_fork_marker(&self, root: &Path, name: &str) -> Result<bool> {
        match self {
            SessionStore::Jsonl => Session::has_fork_marker(root, name),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => session.has_fork_marker().await,
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .has_fork_marker()
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// Depth limit for a fork ancestry walk. A chain longer than this is
    /// treated as a cycle and fails closed instead of resolving forever.
    const FORK_CHAIN_LIMIT: usize = 32;

    /// The resolved zero-copy fork transcript, but ONLY when this session
    /// really stores a leading fork marker.
    ///
    /// JSONL can only be parsed in full, so it resolves unconditionally and
    /// the caller reuses that single parse. The SQL backends probe the
    /// session's oldest rows first (a bounded read, never the whole
    /// transcript) and return `None` for an ordinary session, which keeps its
    /// bounded physical page.
    async fn leading_fork_history(&self, root: &Path, name: &str) -> Result<Option<LoadedLocated>> {
        // The marker is the FIRST stored entry (a lone `ForkedFrom`, or the
        // child's frozen `GoalUpdated` right before it), so three LOGICAL
        // entries are enough. The bound is 3 rather than 2 so the probe still
        // sees both when a backend numbers the first row `seq = 1`.
        const PROBE: usize = 3;
        if self.is_jsonl() || self.has_leading_fork_marker(root, name, PROBE).await? {
            // Head-oriented: a fork child that compacted on its own serves its
            // own head, and the inherited context is only materialized when it
            // really is the model context.
            return Ok(Some(self.load_fork_head_located(root, name).await?));
        }
        Ok(None)
    }

    /// Bounded probe: does this session's stored log BEGIN with its own
    /// zero-copy fork marker?
    ///
    /// It reads the first `bound` LOGICAL entries — the rows with
    /// `seq < bound + 1`, folded by the same same-seq winner logic as every
    /// other load — so an ordinary session never pays for its whole
    /// transcript, and duplicate physical retry rows can never consume the
    /// probe and hide the leading marker (a segment page would: its LIMIT
    /// counts physical versions).
    async fn has_leading_fork_marker(&self, root: &Path, name: &str, bound: usize) -> Result<bool> {
        let entries = self.load_first_entries(root, name, bound).await?;
        Ok(zero_copy_marker(&entries).is_some())
    }

    /// [`Self::load_fork_located`] paired with each entry's backend `seq`
    /// (the JSONL ordinal or the SQL `seq` column; the entry index when the
    /// entry has no child location, i.e. an inherited prefix entry). Used by
    /// the fork-boundary scans (TUI `/fork`, Web fork-candidates) so `at`
    /// always indexes the RESOLVED transcript the fork itself re-loads.
    pub async fn load_fork_with_seq(
        &self,
        root: &Path,
        name: &str,
    ) -> Result<Vec<(i64, SessionEntry)>> {
        let loaded = self.load_fork_located(root, name).await?;
        let seqs: Vec<i64> = loaded
            .entries
            .iter()
            .enumerate()
            .map(|(index, _)| {
                loaded
                    .locations
                    .get(index)
                    .and_then(|location| location.as_ref())
                    .map_or(index as i64, |location| location.key.seq())
            })
            .collect();
        Ok(seqs.into_iter().zip(loaded.entries).collect())
    }

    /// Exact-version field read: resolve a verified `eout1` receipt to the
    /// exact persisted bytes of the field it names. The backend pins the
    /// location (`seq`+`event_time` / `ordinal`), re-checks the entry hash
    /// (a same-seq later version can never retarget an old ref), verifies
    /// the fingerprint/session binding, extracts the field, and rejects any
    /// total-size drift with an `integrity` error. The caller (the runner's
    /// `read_output`) additionally checks the receipt names the current
    /// session before calling.
    pub async fn read_field(
        &self,
        root: &Path,
        verified: &crate::output_receipt::VerifiedRef,
    ) -> Result<Vec<u8>> {
        match self {
            SessionStore::Jsonl => Session::read_field(root, verified),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => session.read_field(verified).await,
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .read_field(verified)
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// Direct current-session field read for a durable numeric output ref.
    /// The backend/session are selected only by this already-bound store.
    pub async fn read_field_direct(
        &self,
        root: &Path,
        session_id: &str,
        entry_id: i64,
        field: crate::output_receipt::FieldId,
    ) -> Result<Vec<u8>> {
        match self {
            SessionStore::Jsonl => Session::read_field_direct(root, session_id, entry_id, field),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                session.read_field_direct(entry_id, field).await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .read_field_direct(entry_id, field)
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// Record one token-usage row in the `usage_entries` table. `kind` is
    /// one of "regular" (normal turn), "compact" (compaction) or
    /// "summarizer" (desktop-pet summary model). The optional
    /// cache/reasoning/finish-reason fields of `usage` are written when
    /// the provider reported them, NULL otherwise.
    ///
    /// `seq` is the `session_entries.seq` of the just-committed assistant /
    /// compaction entry this usage belongs to (threaded from the runner's
    /// commit result), so `usage_entries.seq` EQUALS the corresponding
    /// `session_entries.seq` — a real equality join, not a heuristic. For
    /// calls with no committed entry ("summarizer", or a `None` fallback)
    /// the backend substitutes its strictly monotonic event-time clock and
    /// the row is only event-time-ordered, never seq-joinable.
    ///
    /// For `Jsonl` this is a silent no-op: the file backend has no usage
    /// table, so token statistics are only available on the Greptime/
    /// SQLite backends (the CLI `usage` subcommand prints a hint when the
    /// workspace uses the JSONL backend).
    #[allow(unused_variables)]
    pub async fn append_usage(
        &self,
        root: &Path,
        session_id: &str,
        model: &str,
        kind: &str,
        seq: Option<i64>,
        usage: &crate::agent::Usage,
    ) -> Result<()> {
        match self {
            SessionStore::Jsonl => Ok(()),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                let workspace_id = derive_workspace_id(root);
                session
                    .append_usage(&workspace_id, session_id, model, kind, seq, usage)
                    .await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => {
                let workspace_id = derive_workspace_id(root);
                session
                    .lock()
                    .await
                    .append_usage(&workspace_id, session_id, model, kind, seq, usage)
                    .await
                    .map_err(anyhow::Error::msg)
            }
        }
    }

    /// List the most recent finished background tasks for the workspace,
    /// newest first, by reading `session_entries` rows of
    /// `entry_kind = 'background_completion'` (the ONLY authoritative
    /// record — there is no separate finished-task store). `limit` caps
    /// the result (a sane UI bound, e.g. 100).
    ///
    /// `Jsonl` returns an empty vector: the file backend has no
    /// `session_entries` table to query (same limitation as
    /// [`SessionStore::usage_summary`]); the live TUI/web views still show
    /// finished tasks through the completion events, but the DB-backed
    /// finished-tasks listing requires Greptime/SQLite.
    #[allow(unused_variables)]
    pub async fn finished_tasks(&self, root: &Path, limit: usize) -> Result<Vec<FinishedTask>> {
        let workspace_id = derive_workspace_id(root);
        match self {
            SessionStore::Jsonl => Ok(Vec::new()),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                session.finished_tasks(&workspace_id, limit).await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .finished_tasks(&workspace_id, limit)
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// Aggregate token usage per (session, model, kind) for the workspace.
    /// `Jsonl` returns an empty vector (no usage table on the file
    /// backend).
    #[allow(unused_variables)]
    pub async fn usage_summary(&self, root: &Path) -> Result<Vec<UsageRow>> {
        match self {
            SessionStore::Jsonl => Ok(Vec::new()),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => session.usage_summary().await,
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .usage_summary()
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// Aggregate token usage restricted to the given session ids (a
    /// session plus its subagent children) — the persisted source of the
    /// web UI's per-session usage line (`GET /api/sessions/{id}/usage`),
    /// which must survive a server restart (live in-process counters do
    /// not). `Jsonl` returns an empty vector (no usage table on the file
    /// backend); an empty `session_ids` slice short-circuits to empty.
    #[allow(unused_variables)]
    pub async fn usage_for_sessions(
        &self,
        root: &Path,
        session_ids: &[String],
    ) -> Result<Vec<UsageRow>> {
        match self {
            SessionStore::Jsonl => Ok(Vec::new()),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => session.usage_for_sessions(session_ids).await,
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .usage_for_sessions(session_ids)
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// Aggregate dashboard rows in the requested half-open window. The
    /// caller supplies the already-resolved root/direct-child scope.
    #[allow(unused_variables)]
    pub async fn usage_dashboard(
        &self,
        root: &Path,
        session_ids: &[String],
        from_us: i64,
        to_us: i64,
        bucket_us: i64,
    ) -> Result<Vec<UsageDashboardRow>> {
        match self {
            SessionStore::Jsonl => Ok(Vec::new()),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                session
                    .usage_dashboard(session_ids, from_us, to_us, bucket_us)
                    .await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .usage_dashboard(session_ids, from_us, to_us, bucket_us)
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// Fetch only the metadata projection needed to enrich usage rows. This
    /// intentionally does not expose the full workspace metadata listing.
    #[allow(unused_variables)]
    pub async fn usage_dashboard_metadata(
        &self,
        root: &Path,
        session_ids: &[String],
    ) -> Result<Vec<UsageDashboardMetadata>> {
        match self {
            SessionStore::Jsonl => Ok(Vec::new()),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                session.usage_dashboard_metadata(session_ids).await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .usage_dashboard_metadata(session_ids)
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
    /// I/O is small). Never self-creates on any backend
    /// (R3): a session with no metadata row / sidecar is a no-op.
    pub fn touch_meta(&self, root: &Path, session: &str) {
        match self {
            SessionStore::Jsonl => {
                if let Err(error) = jsonl_touch_meta(root, session) {
                    tracing::warn!("e-agent: cannot touch session metadata: {error:#}");
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
                            if let Err(error) = greptime.touch_meta().await {
                                tracing::warn!("e-agent: cannot touch session metadata: {error:#}");
                            }
                        });
                    }
                    Err(_) => {
                        tracing::warn!("e-agent: cannot touch session metadata: no tokio runtime");
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
                                tracing::warn!("e-agent: cannot touch session metadata: {error:#}");
                            }
                        });
                    }
                    Err(_) => {
                        tracing::warn!("e-agent: cannot touch session metadata: no tokio runtime");
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
    /// `None`). `title` names a session at CREATION time only (delegate /
    /// btw subagents pass their task-panel label): on the existing-row
    /// backfill path an existing title is preserved and `None` means
    /// unnamed — a resume never rewrites a title.
    #[allow(unused_variables, clippy::too_many_arguments)]
    pub async fn create_meta(
        &self,
        root: &Path,
        session: &str,
        model: Option<&str>,
        role: Option<&str>,
        parent_session_id: Option<&str>,
        parent_task_id: Option<i64>,
        title: Option<&str>,
    ) -> Result<()> {
        match self {
            SessionStore::Jsonl => jsonl_create_meta(
                root,
                session,
                model,
                role,
                parent_session_id,
                parent_task_id,
                title,
            ),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime {
                session: greptime_session,
                ..
            } => {
                greptime_session
                    .create_meta(
                        session,
                        model,
                        role,
                        parent_session_id,
                        parent_task_id,
                        title,
                    )
                    .await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite {
                session: sqlite_session,
                ..
            } => sqlite_session
                .lock()
                .await
                .create_meta(
                    session,
                    model,
                    role,
                    parent_session_id,
                    parent_task_id,
                    title,
                )
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// Find child sessions using each session's latest metadata snapshot.
    /// Greptime uses a targeted workspace-scoped query; other backends keep
    /// their existing list-and-filter behavior.
    #[allow(unused_variables)]
    pub async fn child_session_ids(
        &self,
        root: &Path,
        parent_session_id: &str,
    ) -> Result<Vec<String>> {
        match self {
            SessionStore::Jsonl => Ok(jsonl_list_meta(root)?
                .into_iter()
                .filter(|meta| meta.parent_session_id.as_deref() == Some(parent_session_id))
                .map(|meta| meta.session_id)
                .collect()),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                session.child_session_ids(parent_session_id).await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .child_session_ids(parent_session_id)
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// List the latest metadata snapshot per session (Greptime/SQLite:
    /// newest activity first from the audit table; JSONL: one tail-read of
    /// each transcript's sidecar), newest activity first. This is the
    /// unfiltered data API; the server's sidebar read path is
    /// [`Self::list_meta_with_diagnostics`] with the optional live-session
    /// filter.
    pub async fn list_meta(&self, root: &Path) -> Result<Vec<SessionMeta>> {
        match self {
            SessionStore::Jsonl => jsonl_list_meta(root),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => session.list_meta().await,
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .list_meta()
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// Connect a request-local Greptime read session without setup. Other
    /// backends retain their existing shared/store behavior.
    #[cfg(feature = "greptime")]
    pub async fn connect_meta_read_only(backend: &SessionBackend, root: &Path) -> Result<Self> {
        let SessionBackend::Greptime { conn } = backend else {
            anyhow::bail!("read-only Greptime store requires the Greptime backend");
        };
        let session = crate::session_greptime::GreptimeSession::connect_read_only(
            conn,
            &derive_workspace_id(root),
            META_STORE_SENTINEL,
        )
        .await?;
        Ok(SessionStore::Greptime {
            session: Arc::new(session),
            conn: conn.clone(),
        })
    }

    /// List metadata and collect backend timings for the server's slow-path
    /// diagnostic. The ordinary `list_meta` API above remains data-only.
    ///
    /// `sidebar_filter` is the sidebar's live-session id set
    /// (`?exclude=archived_children`): `Some` pushes the filter down to the
    /// backend (Greptime/SQLite: SQL on the current snapshot; JSONL: the
    /// sidecar scan), `None` keeps the full list. The filter runs on meta
    /// snapshots only — live handles are merged in by the server afterwards.
    pub async fn list_meta_with_diagnostics(
        &self,
        root: &Path,
        sidebar_filter: Option<&HashSet<String>>,
    ) -> Result<(Vec<SessionMeta>, ListMetaDiagnostics)> {
        match self {
            SessionStore::Jsonl => {
                let started = std::time::Instant::now();
                let (rows, mut diagnostic) = jsonl_list_meta_diagnostic(root, sidebar_filter)?;
                diagnostic.backend_operation_ms = started.elapsed().as_millis();
                Ok((rows, diagnostic))
            }
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                let operation_started = std::time::Instant::now();
                let (rows, mut diagnostic) = session.list_meta_diagnostic(sidebar_filter).await?;
                diagnostic.backend_operation_ms = operation_started.elapsed().as_millis();
                Ok((rows, diagnostic))
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => {
                let started = std::time::Instant::now();
                let guard = session.lock().await;
                let wait_ms = started.elapsed().as_millis();
                let operation_started = std::time::Instant::now();
                let (rows, mut diagnostic) = guard
                    .list_meta_diagnostic(sidebar_filter)
                    .await
                    .map_err(anyhow::Error::msg)?;
                diagnostic.facade_lock_wait_ms = wait_ms;
                diagnostic.backend_operation_ms = operation_started.elapsed().as_millis();
                Ok((rows, diagnostic))
            }
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
            } => greptime_session.delete_meta(session).await,
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
            } => greptime_session.set_title(session, title).await,
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
            } => greptime_session.set_pinned(session, pinned).await,
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
            } => greptime_session.set_archived(session, archived).await,
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

    /// Read → mutate → append the session's PR/issue links, returning the
    /// value that was actually stored (post [`dedup_refs`]). Both writers —
    /// the `link_ref` tool and the web endpoint — go through this one path,
    /// and both reuse the handle already open (never a second connection).
    ///
    /// A missing metadata row fails loudly instead of silently dropping a
    /// link (`set_title` is a no-op there; a link must not be lost quietly).
    #[allow(unused_variables)]
    pub async fn update_refs(
        &self,
        root: &Path,
        session: &str,
        mutate: impl FnOnce(&mut Vec<SessionRef>) -> Result<(), String>,
    ) -> Result<Vec<SessionRef>> {
        match self {
            SessionStore::Jsonl => jsonl_update_refs(root, session, mutate),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime {
                session: greptime_session,
                ..
            } => greptime_session.update_refs(session, mutate).await,
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite {
                session: sqlite_session,
                ..
            } => sqlite_session
                .lock()
                .await
                .update_refs(session, mutate)
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
            SessionStore::Greptime { session, .. } => session.backfill_sessions().await,
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .backfill_sessions()
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// Make the durable task rows equal exactly `tasks` (the caller's live
    /// set), per backend. JSONL rewrites the record file; the DB backends
    /// diff against the current rows so unchanged sets do no I/O. Errors
    /// surface to the caller, which logs and retries — never aborts work.
    pub(crate) async fn project_background_tasks(
        &self,
        root: &Path,
        session: &str,
        tasks: &[ProjectedBackgroundTask],
    ) -> Result<()> {
        match self {
            SessionStore::Jsonl => Session::rewrite_background_tasks(root, session, tasks),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session: db, .. } => {
                let current = db.peek_unfinished_tasks(session).await?;
                let wanted: HashSet<u64> = tasks.iter().map(|t| t.id).collect();
                let have: HashSet<u64> = current.iter().map(|t| t.task_id).collect();
                for task in tasks.iter().filter(|t| !have.contains(&t.id)) {
                    db.record_task_start(
                        session,
                        task.id,
                        &task.label,
                        task.full_command.as_deref(),
                        task.subagent_session_id.as_deref(),
                    )
                    .await?;
                }
                for task in current.iter().filter(|t| !wanted.contains(&t.task_id)) {
                    db.clear_task(session, task.task_id).await?;
                }
                Ok(())
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session: db, .. } => {
                let db = db.lock().await;
                let current = db
                    .peek_unfinished_tasks(session)
                    .await
                    .map_err(anyhow::Error::msg)?;
                let wanted: HashSet<u64> = tasks.iter().map(|t| t.id).collect();
                let have: HashSet<u64> = current.iter().map(|t| t.task_id).collect();
                for task in tasks.iter().filter(|t| !have.contains(&t.id)) {
                    db.record_task_start(
                        session,
                        task.id,
                        &task.label,
                        task.full_command.as_deref(),
                        task.subagent_session_id.as_deref(),
                    )
                    .await
                    .map_err(anyhow::Error::msg)?;
                }
                for task in current.iter().filter(|t| !wanted.contains(&t.task_id)) {
                    db.clear_task(session, task.task_id)
                        .await
                        .map_err(anyhow::Error::msg)?;
                }
                Ok(())
            }
        }
    }

    // ------------------------------------------------------------------
    // Background-task state records
    // ------------------------------------------------------------------
    //
    // Production writes go exclusively through `project_background_tasks`
    // (the runner's reconciliation of its live in-memory registry). The
    // remaining sync `record_background_start` / `clear_background_task`
    // facades are TEST-ONLY seeding helpers.

    /// Test-only seeding helper: append one background-task row directly,
    /// bypassing the projection, so a test can arrange leftover state.
    ///
    /// `label` 是源头截断的 100 字符预览（「被杀」Notice 用）；`full_command`
    /// 是完整命令原文（bash 任务传 `Some`，delegate 无命令传 `None`），
    /// 持久化后供 UI/`task_full_command` 取用。旧记录缺该字段 → 读回 `None`。
    #[cfg(test)]
    pub fn record_background_start(
        &self,
        root: &Path,
        session: &str,
        id: u64,
        label: &str,
        full_command: Option<&str>,
        subagent_session_id: Option<&str>,
    ) {
        match self {
            SessionStore::Jsonl => {
                if let Err(error) = Session::record_background_start(
                    root,
                    session,
                    id,
                    label,
                    full_command,
                    subagent_session_id,
                ) {
                    tracing::warn!("e-agent: cannot record background task: {error:#}");
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
                let full_command = full_command.map(str::to_owned);
                let subagent = subagent_session_id.map(str::to_owned);
                match tokio::runtime::Handle::try_current() {
                    Ok(handle) => {
                        handle.spawn(async move {
                            if let Err(error) = greptime
                                .record_task_start(
                                    &session_id,
                                    id,
                                    &label,
                                    full_command.as_deref(),
                                    subagent.as_deref(),
                                )
                                .await
                            {
                                tracing::warn!("e-agent: cannot record background task: {error:#}");
                            }
                        });
                    }
                    Err(_) => {
                        tracing::warn!("e-agent: cannot record background task: no tokio runtime");
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
                let full_command = full_command.map(str::to_owned);
                let subagent = subagent_session_id.map(str::to_owned);
                match tokio::runtime::Handle::try_current() {
                    Ok(handle) => {
                        handle.spawn(async move {
                            if let Err(error) = sqlite
                                .lock()
                                .await
                                .record_task_start(
                                    &session_id,
                                    id,
                                    &label,
                                    full_command.as_deref(),
                                    subagent.as_deref(),
                                )
                                .await
                            {
                                tracing::warn!("e-agent: cannot record background task: {error:#}");
                            }
                        });
                    }
                    Err(_) => {
                        tracing::warn!("e-agent: cannot record background task: no tokio runtime");
                    }
                }
            }
        }
    }

    /// Test-only seeding helper: forget one task row directly.
    #[cfg(test)]
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
                            if let Err(error) = greptime.clear_task(&session_id, id).await {
                                tracing::warn!("e-agent: cannot clear background task: {error:#}");
                            }
                        });
                    }
                    Err(_) => {
                        tracing::warn!("e-agent: cannot clear background task: no tokio runtime");
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
                                tracing::warn!("e-agent: cannot clear background task: {error:#}");
                            }
                        });
                    }
                    Err(_) => {
                        tracing::warn!("e-agent: cannot clear background task: no tokio runtime");
                    }
                }
            }
        }
    }

    /// Live task rows a previous process left behind when it died before
    /// their completion arrived. Read-only: returns the exact rows so the
    /// caller can inject the "killed on exit" notice and then
    /// [`Self::consume_unfinished_background`] them. Only this session's
    /// own records are returned.
    pub(crate) async fn peek_unfinished_background(
        &self,
        root: &Path,
        session: &str,
    ) -> Result<Vec<UnfinishedTask>> {
        match self {
            SessionStore::Jsonl => Session::peek_unfinished_background(root, session),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime {
                session: greptime_session,
                ..
            } => {
                let session_id = session.to_owned();
                greptime_session.peek_unfinished_tasks(&session_id).await
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
                    .peek_unfinished_tasks(&session_id)
                    .await
                    .map_err(anyhow::Error::msg)
            }
        }
    }

    /// Consume exactly the records represented by a prior peek.
    pub(crate) async fn consume_unfinished_background(
        &self,
        root: &Path,
        session: &str,
        tasks: &[UnfinishedTask],
    ) -> Result<()> {
        match self {
            SessionStore::Jsonl => {
                Session::consume_unfinished_background(root, session, tasks)?;
                Ok(())
            }
            #[cfg(feature = "greptime")]
            SessionStore::Greptime {
                session: backend_session,
                ..
            } => backend_session
                .consume_unfinished_tasks(session, tasks)
                .await
                .map_err(anyhow::Error::msg),
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite {
                session: backend_session,
                ..
            } => backend_session
                .lock()
                .await
                .consume_unfinished_tasks(session, tasks)
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    /// Look up one surviving background-task record's full command by
    /// (session, task_id) — the server's `/api/tasks` fallback when the
    /// live registry's `BackgroundTaskInfo.full_command` is `None`
    /// (restart-stale rows, old-format records): the DB/JSONL has the
    /// persisted command, so the UI can still show it. `None` when the
    /// record is gone (consumed/completed) or was written without a
    /// `full_command` (delegate rows, pre-migration records).
    pub async fn task_full_command(
        &self,
        root: &Path,
        session: &str,
        task_id: u64,
    ) -> Result<Option<String>> {
        match self {
            SessionStore::Jsonl => Ok(Session::task_full_command(root, session, task_id)),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime {
                session: greptime_session,
                ..
            } => {
                let session_id = session.to_owned();
                greptime_session
                    .task_full_command(&session_id, task_id)
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
                    .task_full_command(&session_id, task_id)
                    .await
                    .map_err(anyhow::Error::msg)
            }
        }
    }

    /// The task-panel label for a subagent session: the label of the newest
    /// surviving `running_tasks` row whose `subagent_session_id` matches
    /// (rows are deleted when the delegate task completes, so `None` means
    /// "no live delegate task carries this session" and the frontend falls
    /// back to the session id). JSONL: scans every `<id>.background.jsonl`
    /// record file for a surviving delegate line whose `session_id`
    /// matches — record lines are a projection of the live task set and
    /// disappear on task completion (`project_background_tasks`).
    ///
    /// This is persisted task metadata, not runtime liveness: the record
    /// files / `running_tasks` rows have async write and cleanup windows,
    /// so a surviving label cannot promise a tail-free "the subagent is
    /// alive right now". `list_sessions` treats the label as display-only;
    /// liveness comes from the main registry or a live parent's `Sessions`
    /// registry (real handles).
    pub async fn label_for_subagent(
        &self,
        root: &Path,
        _subagent_session_id: &str,
    ) -> Result<Option<String>> {
        match self {
            SessionStore::Jsonl => Ok(jsonl_label_for_subagent(root, _subagent_session_id)),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                session.label_for_subagent(_subagent_session_id).await
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

    /// The batched form of [`Self::label_for_subagent`], used by the
    /// sessions list to resolve every subagent label in ONE scan instead
    /// of a per-item N+1.
    ///
    /// JSONL: one `read_dir` + one pass over every surviving
    /// `<id>.background.jsonl` record file (spawned on a blocking thread
    /// — sync file I/O must never block the executor), returning the full
    /// `subagent_session_id → label` map.
    ///
    /// Greptime/SQLite: one indexed query over `running_tasks`, returning
    /// the same full map — the batched form of their per-item
    /// [`Self::label_for_subagent`].
    pub async fn all_subagent_labels(
        &self,
        root: &Path,
    ) -> Result<Option<HashMap<String, Option<String>>>> {
        match self {
            SessionStore::Jsonl => {
                let root = root.to_owned();
                tokio::task::spawn_blocking(move || jsonl_all_subagent_labels(&root))
                    .await
                    .map_err(anyhow::Error::from)
                    .map(Some)
            }
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => session.all_subagent_labels().await.map(Some),
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .all_subagent_labels()
                .await
                .map_err(anyhow::Error::msg)
                .map(Some),
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

/// Serialize read-modify-append of ONE session's metadata sidecar inside
/// this process. The sidecar is a full-snapshot log: every mutation reads
/// the newest line, mutates it, and appends a new line, so two concurrent
/// writers (two ref edits, or an edit racing a turn-boundary touch) would
/// otherwise both start from the same line and the later append would
/// silently drop the earlier change. Keyed by sidecar path — unrelated
/// sessions never contend; cross-process writers stay last-write-wins (the
/// optimistic semantics the DB backends document). One tiny leaked mutex per
/// session path, bounded by the sessions this process has written.
fn jsonl_meta_lock(path: &Path) -> &'static std::sync::Mutex<()> {
    type Locks = HashMap<PathBuf, &'static std::sync::Mutex<()>>;
    static LOCKS: std::sync::OnceLock<std::sync::Mutex<Locks>> = std::sync::OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    locks
        .entry(path.to_path_buf())
        .or_insert_with(|| Box::leak(Box::new(std::sync::Mutex::new(()))))
}

/// Take the per-sidecar snapshot lock, ignoring poisoning (a panicking
/// writer cannot leave the file half-updated: every append is one write).
fn lock_jsonl_meta(path: &Path) -> std::sync::MutexGuard<'static, ()> {
    jsonl_meta_lock(path)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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
/// sidecar does not exist or holds no decodable record. Corrupt records are
/// skipped: the file is append-only and only the tail matters (a corrupt
/// tail is treated as "no newer snapshot").
///
/// ONLY `NotFound` means "no sidecar". Any other open/read error is
/// propagated: fork provenance must never read an unreadable sidecar as a
/// session without a parent (that is exactly how an unreadable child
/// regained `link_ref`).
fn jsonl_read_meta_snapshot(root: &Path, session: &str) -> anyhow::Result<Option<SessionMeta>> {
    Ok(jsonl_read_meta_snapshot_and_unknown(root, session)?.0)
}

/// [`jsonl_read_meta_snapshot`] plus whether provenance must be treated as
/// UNKNOWN: the sidecar exists but no record in it can prove parentlessness.
/// That covers a record that failed to decode (a torn crash tail) and a
/// sidecar with no nonblank record at all — the writer creates the file
/// BEFORE its first snapshot, so an interruption between those two
/// operations leaves a zero-byte file, not "no sidecar".
///
/// The last good snapshot still opens the session for DISPLAY (a torn tail
/// is a crash artifact), but provenance must know the read was incomplete:
/// an undecodable or missing record means "unknown", never "this session has
/// no parent".
///
/// Records are read as BYTES, not with `lines()`: `lines()` requires every
/// line to be valid UTF-8, so a write that stopped inside a multi-byte
/// character would fail the whole reopen and block a session whose history
/// is intact. `serde_json::from_slice` puts a mid-character tear in the same
/// "torn record" bucket as a torn brace, while genuine I/O errors still
/// propagate.
fn jsonl_read_meta_snapshot_and_unknown(
    root: &Path,
    session: &str,
) -> anyhow::Result<(Option<SessionMeta>, bool)> {
    let path = jsonl_meta_sidecar_path(root, session)?;
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((None, false)),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot read session metadata {}", path.display()));
        }
    };
    let mut latest: Option<SessionMeta> = None;
    let mut unknown = false;
    let mut saw_record = false;
    for record in std::io::BufReader::new(file).split(b'\n') {
        let record =
            record.with_context(|| format!("cannot read session metadata {}", path.display()))?;
        if record.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        saw_record = true;
        match serde_json::from_slice::<SessionMeta>(&record) {
            Ok(meta) => latest = Some(meta),
            Err(_) => unknown = true,
        }
    }
    Ok((latest, unknown || !saw_record))
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
/// An existing sidecar that holds no decodable record is left untouched,
/// because appending a first line would turn unknown provenance into a
/// parentless one.
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
    title: Option<&str>,
) -> anyhow::Result<()> {
    let _guard = lock_jsonl_meta(&jsonl_meta_sidecar_path(root, session)?);
    let (existing, unknown) = jsonl_read_meta_snapshot_and_unknown(root, session)?;
    if let Some(existing) = existing {
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
                refs: existing.refs.clone(), // links are never rewritten by a resume
                writer: None, // stamped by jsonl_append_meta_snapshot with the writing process
                label: None, // label lives in running_tasks / background records, resolved at list time
            };
            return jsonl_append_meta_snapshot(root, session, &mut meta);
        }
        return Ok(()); // resume: nothing missing, never rewrite created_at
    }
    if unknown {
        // The sidecar EXISTS but no record in it decodes: appending a first
        // line would fabricate parentlessness the bytes never proved, so
        // leave the file (and the unknown provenance) for a real repair.
        return Ok(());
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
        title: title.map(str::to_owned), // a fresh session may be named at creation (subagent label)
        pinned: None,                    // a fresh session is unpinned until the user pins it
        archived: None,                  // a fresh session is unarchived until the user archives it
        refs: Vec::new(),                // no PR/issue links until the tool or the owner adds them
        writer: None, // stamped by jsonl_append_meta_snapshot with the writing process
        label: None,  // label lives in running_tasks / background records, resolved at list time
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
    let _guard = lock_jsonl_meta(&jsonl_meta_sidecar_path(root, session)?);
    let Some(mut meta) = jsonl_read_meta_snapshot(root, session)? else {
        return Ok(()); // R3: never self-create
    };
    mutate(&mut meta);
    meta.last_active_at = chrono::Utc::now().naive_utc();
    jsonl_append_meta_snapshot(root, session, &mut meta)
}

/// `update_refs` for the JSONL backend: read → mutate → append ONE snapshot
/// line under the same per-session lock every other metadata writer takes,
/// so two concurrent adds both survive instead of the later one overwriting
/// the earlier read. Returns the stored (deduplicated) list. A missing
/// sidecar is an error — a link handed to the store must not be dropped
/// quietly (the flag setters' no-op would).
fn jsonl_update_refs(
    root: &Path,
    session: &str,
    mutate: impl FnOnce(&mut Vec<SessionRef>) -> Result<(), String>,
) -> anyhow::Result<Vec<SessionRef>> {
    let _guard = lock_jsonl_meta(&jsonl_meta_sidecar_path(root, session)?);
    let Some(mut meta) = jsonl_read_meta_snapshot(root, session)? else {
        anyhow::bail!("session {session} has no metadata row to store links in");
    };
    mutate(&mut meta.refs).map_err(anyhow::Error::msg)?;
    dedup_refs(&mut meta.refs);
    meta.last_active_at = chrono::Utc::now().naive_utc();
    jsonl_append_meta_snapshot(root, session, &mut meta)?;
    Ok(meta.refs)
}

/// `touch_meta` for the JSONL backend: append one fresh snapshot line with
/// `last_active_at = now` and `entry_count` = the current transcript line
/// count, or no-op when no sidecar exists (R3 — a sidecar-less session
/// must not fabricate its own row). The transcript is re-counted on every
/// call: the store is stateless, and the count is the exact JSONL analogue
/// of the DB's `next_seq`. Called synchronously at turn boundaries; the
/// read is a few hundred KB at worst.
fn jsonl_touch_meta(root: &Path, session: &str) -> anyhow::Result<()> {
    let _guard = lock_jsonl_meta(&jsonl_meta_sidecar_path(root, session)?);
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
    Ok(jsonl_list_meta_diagnostic(root, None)?.0)
}

/// Whether the sidebar's `?exclude=archived_children` set keeps `meta`.
/// Evaluated on the sidecar's LATEST snapshot, before the server merges
/// live handles in: pinned rows and unarchived roots stay, and so does any
/// session in `live` — the server's snapshot of the live registries (main
/// or child), which is what keeps a live child listed even though its
/// `active` flag is only filled in after this filter runs. Mirrors the
/// Greptime/SQLite WHERE predicate (`pinned = true OR (archived IS NOT
/// true AND parent_session_id IS NULL) OR session_id IN (live)`). Pure so
/// the rule is unit-testable without a sidecar.
fn sidebar_filter_keeps(meta: &SessionMeta, live: &HashSet<String>) -> bool {
    meta.pinned == Some(true)
        || (meta.archived != Some(true) && meta.parent_session_id.is_none())
        || live.contains(&meta.session_id)
}

fn jsonl_list_meta_diagnostic(
    root: &Path,
    sidebar_filter: Option<&HashSet<String>>,
) -> anyhow::Result<(Vec<SessionMeta>, ListMetaDiagnostics)> {
    let started = std::time::Instant::now();
    let directory = root.join(".e-agent/sessions");
    let mut diagnostic = ListMetaDiagnostics {
        backend: "jsonl",
        ..Default::default()
    };
    let Ok(entries) = std::fs::read_dir(&directory) else {
        diagnostic.filesystem_parse_ms = started.elapsed().as_millis();
        return Ok((Vec::new(), diagnostic));
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(session_id) = name.strip_suffix(".meta.jsonl") else {
            continue;
        };
        diagnostic.sidecars_seen += 1;
        if let Some(meta) = jsonl_read_meta_snapshot(root, session_id)? {
            diagnostic.sidecars_opened += 1;
            if sidebar_filter.is_none_or(|live| sidebar_filter_keeps(&meta, live)) {
                out.push(meta);
            }
        }
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.last_active_at));
    diagnostic.logical_rows = out.len();
    diagnostic.filesystem_parse_ms = started.elapsed().as_millis();
    Ok((out, diagnostic))
}

/// `delete_meta` for the JSONL backend: remove the session's sidecar file.
/// The transcript stays, so resume still works. A missing sidecar is `Ok`
/// (idempotent, matching the DB's zero-row DELETE). Known limitation
/// (mirroring the audit-log design without tombstones): the next
/// `backfill_sessions` bootstrap run re-creates the file from the
/// transcript, so hiding is scoped to the current server lifetime.
fn jsonl_delete_meta(root: &Path, session: &str) -> anyhow::Result<()> {
    let path = jsonl_meta_sidecar_path(root, session)?;
    // Same per-session snapshot lock as the writers: a delete must not be
    // undone by an in-flight read-modify-append (which would recreate the
    // sidecar with the snapshot it read before the delete).
    let _guard = lock_jsonl_meta(&path);
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
/// on their next resume). A sidecar that exists but cannot decode is left
/// untouched, because backfilling it would fabricate the parentlessness
/// the interrupted write never recorded.
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
        // Same per-session lock as every other metadata writer: another
        // handle may create the sidecar between the absence check and this
        // append, and a second creation line would rewrite created_at and
        // omit links that handle stored. Re-check under the lock.
        let path = jsonl_meta_sidecar_path(root, &session)?;
        let _guard = lock_jsonl_meta(&path);
        let (existing, unknown) = jsonl_read_meta_snapshot_and_unknown(root, &session)?;
        if existing.is_some() {
            continue; // already has a snapshot (idempotent)
        }
        if unknown {
            continue; // interrupted first write: never fabricate a first line
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
            title: None,      // pre-sidecar sessions have no user-assigned name
            pinned: None,     // pre-sidecar sessions are unpinned
            archived: None,   // pre-sidecar sessions are unarchived
            refs: Vec::new(), // pre-sidecar sessions carry no PR/issue links
            writer: None,     // stamped by jsonl_append_meta_snapshot with the writing process
            label: None, // label lives in running_tasks / background records, resolved at list time
        };
        jsonl_append_meta_snapshot(root, &session, &mut meta)?;
    }
    Ok(())
}

/// `label_for_subagent` for the JSONL backend: scan every surviving
/// `<id>.background.jsonl` record file for a line whose `session_id`
/// matches the subagent, and return the last (newest) matching label.
/// Record lines are a projection of the live task set, so they disappear
/// when the delegate task completes (`project_background_tasks`) — the
/// same contract as a `running_tasks` row. Files are scanned in sorted
/// name order and lines in sorted-id order, so "newest" is deterministic;
/// a subagent with several live delegates reports the most recently
/// projected one. Non-destructive (called from the sessions list, like
/// the DB lookup).
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

/// Batch `jsonl_label_for_subagent`: ONE `read_dir` + one pass over every
/// surviving `<id>.background.jsonl` record file, building the full
/// `subagent_session_id → label` map in a single scan. Files are scanned
/// in sorted name order and lines in append order, so a later match
/// overwrites an earlier one — "newest wins", exactly the per-session
/// rule. A subagent with no surviving delegate line is simply absent from
/// the map (the caller reads it as `None`). Non-destructive, like the
/// per-session version; replaces the per-item N+1 read_dir + file parse
/// the sessions list used to do.
fn jsonl_all_subagent_labels(root: &Path) -> HashMap<String, Option<String>> {
    let directory = root.join(".e-agent/sessions");
    let mut labels: HashMap<String, Option<String>> = HashMap::new();
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return labels;
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
            if let Some(subagent_id) = record["session_id"].as_str()
                && let Some(text) = record["label"].as_str()
            {
                labels.insert(subagent_id.to_owned(), Some(text.to_owned()));
            }
        }
    }
    labels
}

// ----------------------------------------------------------------------
// Direct unit tests for the shared backend helpers extracted from
// `session_greptime` / `session_sqlite` (P0+P1 refactor). The two backend
// test files exercise `dedup_raw_entries` end-to-end; this module pins the
// extracted pure functions themselves so the shared copies stay covered
// even if one backend's tests are feature-gated away.
// ----------------------------------------------------------------------
#[cfg(test)]
mod shared_helpers {
    use super::*;
    use crate::agent::{AssistantMessage, Message};

    // --- us_to_datetime / datetime_to_us --------------------------------

    #[test]
    fn us_to_datetime_known_fixed_timestamps() {
        // Epoch and a fractional second.
        assert_eq!(
            us_to_datetime(0),
            chrono::NaiveDate::from_ymd_opt(1970, 1, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
        );
        assert_eq!(
            us_to_datetime(1_000_001),
            chrono::NaiveDate::from_ymd_opt(1970, 1, 1)
                .unwrap()
                .and_hms_micro_opt(0, 0, 1, 1)
                .unwrap()
        );
        // 2000-01-01T00:00:00Z = 946684800s.
        assert_eq!(
            us_to_datetime(946_684_800_000_000),
            chrono::NaiveDate::from_ymd_opt(2000, 1, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
        );
        // One second before epoch (negative whole second).
        assert_eq!(
            us_to_datetime(-1_000_000),
            chrono::NaiveDate::from_ymd_opt(1969, 12, 31)
                .unwrap()
                .and_hms_opt(23, 59, 59)
                .unwrap()
        );
    }

    #[test]
    fn us_datetime_roundtrip_boundaries() {
        // us → datetime → us is the identity for every value the backends
        // can produce (all timestamps originate from `next_event_time_us`,
        // so ≥ 0; negative whole-second values convert losslessly too).
        let cases = [
            0i64,
            1,
            999_999,
            1_000_000,
            946_684_800_000_000,     // 2000-01-01
            253_402_300_799_999_999, // 9999-12-31T23:59:59.999999 (chrono max)
            -1,                      // 1969-12-31T23:59:59.999999 (negative sub-second)
            -999_999,                // 1969-12-31T23:59:59.000001 (negative sub-second floor)
            -1_000_000,              // 1969-12-31T23:59:59
            -62_167_219_200_000_000, // 0000-01-01T00:00:00 (chrono min)
        ];
        for us in cases {
            let dt = us_to_datetime(us);
            assert_eq!(
                datetime_to_us(dt),
                us,
                "us→datetime→us must round-trip for {us}"
            );
        }
    }

    #[test]
    fn us_to_datetime_negative_subsecond_does_not_panic() {
        // Regression: the old `us % 1_000_000` formulation wrapped the
        // negative remainder into a huge u32 nanos, `from_timestamp`
        // returned None and the unwrap panicked for any negative
        // sub-second value. `from_timestamp_micros` handles negatives
        // natively, so -1 (and the -999999 sub-second floor) must convert
        // and round-trip exactly.
        let min_one = us_to_datetime(-1);
        assert_eq!(
            min_one,
            chrono::NaiveDate::from_ymd_opt(1969, 12, 31)
                .unwrap()
                .and_hms_micro_opt(23, 59, 59, 999_999)
                .unwrap(),
            "-1 µs = one microsecond before epoch"
        );
        assert_eq!(datetime_to_us(min_one), -1, "round-trip -1");

        let floor = us_to_datetime(-999_999);
        assert_eq!(
            datetime_to_us(floor),
            -999_999,
            "round-trip -999999 (negative sub-second floor)"
        );
        assert!(
            datetime_to_us(us_to_datetime(-1)) < 0 && datetime_to_us(us_to_datetime(-999_999)) < 0,
            "negative sub-second inputs must stay negative through the conversion"
        );
    }

    // --- finished-task logical-row fold --------------------------------

    fn bg_completion_payload(id: u64, status: &str) -> String {
        serde_json::to_string(&SessionEntry::BackgroundCompletion {
            id,
            output: format!("exit code: 0\nstdout:\n{status}\nstderr:\n"),
            label: None,
            started_at_ms: None,
            duration_ms: None,
            exit_code: Some(0),
            signal: None,
            status: Some(status.into()),
            kind: Some("bash".into()),
            cancellation_source: None,
        })
        .unwrap()
    }

    fn finished_time(day: i64) -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 8, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            + chrono::Duration::days(day)
    }

    #[test]
    fn dedup_finished_rows_folds_retries_and_supersedes() {
        let ws = "ws-dedup";
        let payload_a = bg_completion_payload(1, "completed");
        let payload_b = bg_completion_payload(2, "failed");
        let payload_a_superseded = bg_completion_payload(1, "killed");

        // A committed-then-retried completion: same (session, seq) with an
        // identical payload at the same microsecond (idempotent retry), plus
        // a superseded older physical row at a smaller event_time.
        let raw = vec![
            ("sess-a".to_string(), 5, finished_time(1), payload_a.clone()),
            ("sess-a".to_string(), 5, finished_time(1), payload_a.clone()),
            (
                "sess-a".to_string(),
                5,
                finished_time(2),
                payload_a_superseded.clone(),
            ),
            ("sess-b".to_string(), 3, finished_time(3), payload_b.clone()),
        ];
        let folded = dedup_finished_rows(ws, &raw).unwrap();
        assert_eq!(folded.len(), 2, "3 physical rows of (sess-a,5) fold to 1");
        // Newest logical row first (event_time DESC, seq DESC).
        assert_eq!(folded[0].0, "sess-b");
        assert_eq!(folded[1].0, "sess-a");
        assert_eq!(folded[1].1, 5);
        // The winning payload is the NEWEST event_time's (superseded row
        // discarded), and identical same-time duplicates folded.
        let winner: SessionEntry = serde_json::from_str(&folded[1].3).unwrap();
        let SessionEntry::BackgroundCompletion { status, .. } = winner else {
            panic!("expected BackgroundCompletion");
        };
        assert_eq!(
            status.as_deref(),
            Some("killed"),
            "superseded row must not win"
        );
        assert_eq!(
            folded[1].2,
            finished_time(2),
            "winning event_time is the newest"
        );
    }

    #[test]
    fn dedup_finished_rows_fails_closed_on_divergent_ties() {
        let ws = "ws-divergent";
        // Same (session, seq), same max event_time, DIFFERENT payloads →
        // fail closed, exactly like the session load path.
        let raw = vec![
            (
                "sess-a".to_string(),
                5,
                finished_time(1),
                bg_completion_payload(1, "completed"),
            ),
            (
                "sess-a".to_string(),
                5,
                finished_time(1),
                bg_completion_payload(9, "failed"),
            ),
        ];
        let error = dedup_finished_rows(ws, &raw).unwrap_err();
        assert!(error.contains("sess-a"), "{error}");
        assert!(error.contains("seq 5"), "{error}");
        assert!(error.contains("divergent"), "{error}");
    }

    // --- next_event_time_us ---------------------------------------------

    #[test]
    fn next_event_time_us_is_strictly_monotonic() {
        let mut prev = next_event_time_us();
        for _ in 0..1000 {
            let next = next_event_time_us();
            assert!(next > prev, "must be strictly increasing: {next} <= {prev}");
            prev = next;
        }
    }

    #[test]
    fn next_event_time_us_monotonic_across_threads() {
        // The shared AtomicI64 is process-wide by design (one monotonic
        // clock for every backend caller); hammer it from several threads
        // and verify the union of returned values is strictly ordered, so
        // no interleaving can ever produce a non-monotonic pair.
        let handles: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(|| {
                    let mut seq = Vec::with_capacity(50);
                    for _ in 0..50 {
                        seq.push(next_event_time_us());
                    }
                    seq
                })
            })
            .collect();
        let mut all: Vec<i64> = handles
            .into_iter()
            .flat_map(|h| h.join().expect("thread"))
            .collect();
        all.sort_unstable();
        for pair in all.windows(2) {
            assert!(
                pair[1] > pair[0],
                "union of concurrent next_event_time_us must be strictly increasing"
            );
        }
    }

    #[test]
    fn next_event_time_us_composes_with_us_to_datetime() {
        // Every timestamp the backends store must convert losslessly: the
        // round-trip through the chrono layer is exact (whole microseconds).
        for _ in 0..100 {
            let us = next_event_time_us();
            let dt = us_to_datetime(us);
            assert_eq!(
                datetime_to_us(dt),
                us,
                "next_event_time_us output must survive the datetime round-trip"
            );
        }
    }

    // --- derive_workspace_id --------------------------------------------

    #[test]
    fn derive_workspace_id_is_deterministic_and_distinct() {
        let a = Path::new("/tmp/e-agent-ws-alpha");
        let b = Path::new("/tmp/e-agent-ws-beta");
        assert_eq!(
            derive_workspace_id(a),
            derive_workspace_id(a),
            "same root → same id"
        );
        assert_eq!(
            derive_workspace_id(b),
            derive_workspace_id(b),
            "same root → same id"
        );
        assert_ne!(
            derive_workspace_id(a),
            derive_workspace_id(b),
            "different roots → different ids"
        );
        // Sub-path roots are distinct namespaces (matching the JSONL
        // on-disk directory isolation).
        assert_ne!(
            derive_workspace_id(Path::new("/tmp/ws")),
            derive_workspace_id(Path::new("/tmp/ws/sub"))
        );
    }

    #[test]
    fn derive_workspace_id_handles_empty_and_special_roots() {
        // Empty root: must not panic; deterministic.
        let empty = Path::new("");
        let first = derive_workspace_id(empty);
        assert_eq!(first, derive_workspace_id(empty));
        // Whitespace / unicode / non-UTF8 bytes: lossy conversion is
        // deterministic and must not panic.
        let fancy = Path::new("/tmp/会话 dir/ünïcode name");
        assert_eq!(derive_workspace_id(fancy), derive_workspace_id(fancy));
        assert!(derive_workspace_id(fancy).contains("会话"));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let non_utf8 = std::path::PathBuf::from("/tmp/")
                .join(std::ffi::OsStr::from_bytes(b"bad-\xff\xfe-name"));
            assert_eq!(
                derive_workspace_id(&non_utf8),
                derive_workspace_id(&non_utf8)
            );
        }
    }

    #[test]
    fn resolve_sqlite_path_resolves_default_inside_workspace() {
        let root = Path::new("/tmp/ws");
        // None (default backend / no path) -> <root>/.e-agent/sessions.db
        assert_eq!(
            resolve_sqlite_path(root, None),
            "/tmp/ws/.e-agent/sessions.db"
        );
        // Explicit path wins verbatim (no ~ expansion, no rewriting).
        assert_eq!(
            resolve_sqlite_path(root, Some("/data/custom.db")),
            "/data/custom.db"
        );
        // Empty/whitespace path behaves like None (default).
        assert_eq!(
            resolve_sqlite_path(root, Some("   ")),
            "/tmp/ws/.e-agent/sessions.db"
        );
        // Relative explicit path is kept as-is (caller's responsibility);
        // only the default is derived from the workspace root.
        assert_eq!(resolve_sqlite_path(root, Some("rel.db")), "rel.db");
    }

    // --- entry_kind / is_error ------------------------------------------

    fn sample_entries() -> Vec<SessionEntry> {
        vec![
            Message::System {
                content: "sys".into(),
            }
            .into(),
            Message::User {
                content: "hi".into(),
                images: vec![],
            }
            .into(),
            Message::Assistant(AssistantMessage {
                content: Some("answer".into()),
                tool_calls: vec![],
                reasoning: None,
            })
            .into(),
            Message::Tool {
                call_id: "c1".into(),
                name: "bash".into(),
                content: "ok".into(),
                is_error: false,
                synthetic: false,
                images: vec![],
            }
            .into(),
            Message::Tool {
                call_id: "c2".into(),
                name: "bash".into(),
                content: "boom".into(),
                is_error: true,
                synthetic: false,
                images: vec![],
            }
            .into(),
            SessionEntry::Compaction {
                summary: "comp".into(),
                retained: vec![],
                current_prompt_at: None,
                no_current_prompt: false,
            },
            SessionEntry::Notice {
                text: "notice".into(),
            },
            SessionEntry::BackgroundCompletion {
                id: 7,
                output: "done".into(),
                label: None,
                started_at_ms: None,
                duration_ms: None,
                exit_code: None,
                signal: None,
                status: None,
                kind: None,
                cancellation_source: None,
            },
            SessionEntry::ForkedFrom {
                source: "src".into(),
                at: Some(3),
                event_time: None,
                seq: None,
            },
            SessionEntry::Error {
                text: "harness exploded".into(),
            },
        ]
    }

    #[test]
    fn decode_history_rows_only_decodes_selected_winner_ties() {
        let time = us_to_datetime(1);
        let notice = r#"{"type":"notice","text":"selected"}"#.to_owned();
        let selected = decode_history_rows(
            vec![
                ("w".into(), "s".into(), 2, time, notice.clone()),
                ("w".into(), "s".into(), 2, time, notice),
            ],
            "event_time_us",
        )
        .unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].seq, 2);
        let error = decode_history_rows(
            vec![
                (
                    "w".into(),
                    "s".into(),
                    2,
                    time,
                    r#"{"type":"notice","text":"one"}"#.into(),
                ),
                (
                    "w".into(),
                    "s".into(),
                    2,
                    time,
                    r#"{"type":"notice","text":"two"}"#.into(),
                ),
            ],
            "event_time_us",
        )
        .unwrap_err();
        assert!(error.contains("divergent physical duplicates"));
    }

    #[test]
    fn entry_kind_classifies_every_variant() {
        let entries = sample_entries();
        let kinds: Vec<&str> = entries.iter().map(entry_kind).collect();
        assert_eq!(
            kinds,
            vec![
                "message",
                "message",
                "message",
                "message",
                "message", // tool messages are still `message` kind
                "compaction",
                "notice",
                "background_completion",
                "forked_from",
                "error",
            ]
        );
    }

    #[test]
    fn is_error_only_flags_tool_errors() {
        let entries = sample_entries();
        let flags: Vec<bool> = entries.iter().map(is_error).collect();
        assert_eq!(
            flags,
            vec![
                false, false, false, false, true, false, false, false, false, true
            ]
        );
    }

    // --- format_conflict_error ------------------------------------------

    #[test]
    fn format_conflict_error_keeps_the_main_contract_substring() {
        // `main::friendly_failure` matches on the fixed substring; every
        // variant of the message must carry it.
        let with_writer = format_conflict_error("sess-1", 42, 40, 41, Some("w@h#1"));
        assert!(
            with_writer.contains("concurrent write conflict"),
            "contract substring missing: {with_writer}"
        );
        let without_writer = format_conflict_error("sess-1", 42, 40, 41, None);
        assert!(
            without_writer.contains("concurrent write conflict"),
            "contract substring missing: {without_writer}"
        );
    }

    #[test]
    fn format_conflict_error_renders_all_parameters() {
        let msg = format_conflict_error("sess-abc", 99, 90, 95, Some("pid@host#42"));
        assert!(msg.contains("session 'sess-abc'"), "session id: {msg}");
        assert!(msg.contains("max seq is 99"), "db_max: {msg}");
        assert!(
            msg.contains("expected to start at seq 90"),
            "base_seq: {msg}"
        );
        assert!(msg.contains("First divergence at seq 95"), "seq: {msg}");
        assert!(
            msg.contains("latest metadata writer: pid@host#42"),
            "writer hint: {msg}"
        );
        assert!(
            msg.contains("Stop the other writer or continue in a fresh session"),
            "actionable guidance: {msg}"
        );
    }

    #[test]
    fn format_conflict_error_writer_hint_optional() {
        let plain = format_conflict_error("sess-1", 1, 0, 0, None);
        assert!(
            !plain.contains("latest metadata writer"),
            "no writer hint when lookup failed: {plain}"
        );
        // The no-hint variant is otherwise identical to the hint variant
        // minus the trailing clause.
        let with_hint = format_conflict_error("sess-1", 1, 0, 0, Some("other@h#9"));
        assert!(plain.len() < with_hint.len());
        assert!(
            plain.starts_with(
                &with_hint[..with_hint.len() - " ; latest metadata writer: other@h#9".len()]
            ),
            "hint must be a pure suffix"
        );
    }

    // --- dedup_raw_entries (shared-copy edges not covered by backend
    //     suites: gapped seqs and the P1a column parameter) ---------------

    fn et(sec: i64) -> chrono::NaiveDateTime {
        chrono::DateTime::from_timestamp(sec, 0)
            .unwrap()
            .naive_utc()
    }

    fn msg(content: &str) -> String {
        serde_json::to_string(&SessionEntry::from(Message::User {
            content: content.to_owned(),
            images: vec![],
        }))
        .unwrap()
    }

    #[test]
    fn dedup_gapped_seqs_are_preserved_not_errors() {
        // Seqs 0, 2, 5 (gaps): every seq survives, ordered by winning
        // event_time; a missing seq is not an error by itself.
        let raw = vec![
            (0i64, et(100), msg("a")),
            (2i64, et(300), msg("b")),
            (5i64, et(200), msg("c")),
        ];
        let out = dedup_raw_entries(&raw, "s", "w", "event_time").unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].0, 0, "earliest event_time first");
        assert_eq!(out[1].0, 5, "event_time order, not seq order");
        assert_eq!(out[2].0, 2);
    }

    #[test]
    fn dedup_guidance_renders_the_parameterized_event_time_column() {
        // P1a: the inspection SQL names each backend's real timestamp
        // column. The divergent-duplicates guidance must contain exactly
        // the column the caller passed — and both the session id and the
        // workspace id must render into the message (they scope the
        // manual-inspection SQL the user is told to run).
        let raw = vec![(3i64, et(500), msg("hello")), (3i64, et(500), msg("world"))];
        let err_greptime =
            dedup_raw_entries(&raw, "test-sess-1", "test-ws-1", "event_time").unwrap_err();
        assert!(
            err_greptime.contains("ORDER BY event_time;"),
            "greptime column name must appear: {err_greptime}"
        );
        assert!(!err_greptime.contains("event_time_us"));
        assert!(
            err_greptime.contains("test-sess-1"),
            "session id must render into the guidance: {err_greptime}"
        );
        assert!(
            err_greptime.contains("test-ws-1"),
            "workspace id must render into the guidance: {err_greptime}"
        );

        let err_sqlite =
            dedup_raw_entries(&raw, "test-sess-1", "test-ws-1", "event_time_us").unwrap_err();
        assert!(
            err_sqlite.contains("ORDER BY event_time_us;"),
            "sqlite column name must appear: {err_sqlite}"
        );
        assert!(
            err_sqlite.contains("test-sess-1"),
            "session id must render into the guidance: {err_sqlite}"
        );
        assert!(
            err_sqlite.contains("test-ws-1"),
            "workspace id must render into the guidance: {err_sqlite}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ref URL must carry an EXPLICIT `scheme://` authority in the original
    /// string. `Url::parse` REPAIRS a scheme without one (`https:example.org/
    /// pull/14` becomes the path `example.org/pull/14` on host "https"), so the
    /// old checks saw a host and passed while the raw string — the one stored
    /// in `href` and resolved against the UI origin by the browser — had no
    /// origin at all. Every malformed form is refused; an accepted URL keeps
    /// its exact string (the URL is the identity).
    #[test]
    fn ref_urls_require_an_explicit_authority_and_keep_their_exact_string() {
        for (bad, why) in [
            ("https:example.org/pull/14", "scheme without `//`"),
            ("http:example.org/x", "scheme without `//`"),
            ("https:/example.org/x", "single slash"),
            ("https:///example.org/x", "empty authority"),
            ("https://\\example.org/pull/14", "backslash authority"),
            ("https://", "no authority"),
            ("//example.org/x", "no scheme"),
            ("example.org/pull/14", "relative path"),
            ("ftp://example.org/x", "wrong scheme"),
            ("https://user:pw@example.org/x", "credentials"),
            ("https://example.org /x", "whitespace"),
            ("", "empty"),
        ] {
            let error = SessionRef::new("pr", bad).expect_err(why);
            assert!(!error.is_empty(), "{why}: {error}");
        }
        let accepted = "https://Example.org/PULL/14";
        assert_eq!(
            SessionRef::new("pr", accepted).unwrap().url,
            accepted,
            "an accepted URL keeps its exact string"
        );
    }

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

    /// JSONL `count_entries` and `all_subagent_labels` (the sessions-list
    /// hot path): counts are line-counts without a transcript parse, and
    /// the batch label map matches the per-session lookup, newest wins.
    #[tokio::test]
    async fn jsonl_count_entries_and_all_subagent_labels() {
        use crate::agent::Message;
        use crate::session::Session;

        let root = std::env::temp_dir();
        let session = format!("test-jsonl-meta-{}", crate::session::new_id());
        let store = SessionStore::Jsonl;

        // No transcript yet → 0, no parse.
        assert_eq!(
            store.count_entries(&root, &session).await.unwrap(),
            0,
            "missing transcript counts as 0"
        );

        // Appended entries are counted; an unrelated `.meta.jsonl` sidecar
        // must not inflate the count (only `<id>.jsonl` is scanned).
        let entries: Vec<SessionEntry> = (0..3)
            .map(|i| SessionEntry::Message {
                message: Message::User {
                    content: format!("m{i}"),
                    images: vec![],
                },
            })
            .collect();
        Session::append(&root, &session, &entries).unwrap();
        assert_eq!(store.count_entries(&root, &session).await.unwrap(), 3);

        // Batch labels: one scan resolves every subagent. Two parents each
        // record a delegate; the second record for the same subagent
        // (newest line) wins.
        Session::record_background_start(&root, "parent-a", 1, "delegate one", None, Some("sub-1"))
            .unwrap();
        Session::record_background_start(&root, "parent-b", 2, "delegate two", None, Some("sub-2"))
            .unwrap();
        Session::record_background_start(
            &root,
            "parent-a",
            3,
            "delegate one v2",
            None,
            Some("sub-1"),
        )
        .unwrap();
        let all = store.all_subagent_labels(&root).await.unwrap().unwrap();
        assert_eq!(all.get("sub-1"), Some(&Some("delegate one v2".to_owned())));
        assert_eq!(all.get("sub-2"), Some(&Some("delegate two".to_owned())));
        assert_eq!(all.get("sub-missing"), None, "absent = no label");

        // The batch map agrees with the per-session lookup.
        assert_eq!(
            store.label_for_subagent(&root, "sub-1").await.unwrap(),
            Some("delegate one v2".to_owned())
        );
        assert_eq!(
            store.label_for_subagent(&root, "sub-2").await.unwrap(),
            Some("delegate two".to_owned())
        );
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
                path: Some(path.to_string_lossy().into_owned()),
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

        /// Wait for the seeded background row to land: the test-only
        /// `record_background_start` facade spawns its write onto the current
        /// runtime and is never awaited, so poll peek+consume until the row
        /// appears or a deadline passes.
        async fn take_unfinished_with_retry(
            store: &SessionStore,
            root: &Path,
            session: &str,
        ) -> Vec<String> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let tasks = store
                    .peek_unfinished_background(root, session)
                    .await
                    .expect("peek unfinished background");
                let labels = tasks
                    .iter()
                    .map(|t| {
                        crate::session::format_unfinished(
                            t.task_id,
                            &t.label,
                            t.subagent_session_id.as_deref(),
                        )
                    })
                    .collect::<Vec<_>>();
                if !tasks.is_empty() {
                    store
                        .consume_unfinished_background(root, session, &tasks)
                        .await
                        .expect("consume unfinished background");
                }
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
        async fn sqlite_background_task_projection_orders_insert_and_delete() {
            let (_dir, path) = temp_db();
            let root = std::env::temp_dir();
            let session = format!("test-store-bg-durable-{}", crate::session::new_id());
            let store = SessionStore::connect(&backend(&path), &root, &session)
                .await
                .expect("connect sqlite store");

            store
                .project_background_tasks(
                    &root,
                    &session,
                    &[ProjectedBackgroundTask {
                        id: 17,
                        label: "cargo test".into(),
                        full_command: Some("cargo test".into()),
                        subagent_session_id: None,
                    }],
                )
                .await
                .expect("project start");
            let tasks = store
                .peek_unfinished_background(&root, &session)
                .await
                .expect("peek immediately after start");
            assert_eq!(tasks.len(), 1);
            assert_eq!(tasks[0].task_id, 17);

            store
                .project_background_tasks(&root, &session, &[])
                .await
                .expect("project clear");
            assert!(
                store
                    .peek_unfinished_background(&root, &session)
                    .await
                    .expect("peek immediately after clear")
                    .is_empty()
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
            store.record_background_start(&root, &session, 7, "build project", None, None);
            let labels = take_unfinished_with_retry(&store, &root, &session).await;
            assert_eq!(
                labels,
                vec![crate::session::format_unfinished(7, "build project", None)]
            );

            // A second record + clear: clearing forgets the task, so the
            // next take reports nothing. Both record and clear are
            // fire-and-forget spawns on the current runtime; give each
            // spawn a yield window to run to completion before asserting.
            store.record_background_start(&root, &session, 8, "run tests", None, None);
            let landed = take_unfinished_with_retry(&store, &root, &session).await;
            assert_eq!(
                landed,
                vec![crate::session::format_unfinished(8, "run tests", None)]
            );

            // Record again, let it land, clear, let the clear land, then
            // nothing may be reported.
            store.record_background_start(&root, &session, 8, "run tests", None, None);
            for _ in 0..1000 {
                tokio::task::yield_now().await;
            }
            store.clear_background_task(&root, &session, 8);
            for _ in 0..1000 {
                tokio::task::yield_now().await;
            }
            let labels = store
                .peek_unfinished_background(&root, &session)
                .await
                .expect("final take");
            assert!(labels.is_empty(), "cleared task must not be reported");

            // A subagent-scoped record is visible through the display-only
            // label lookup, even from a different session's store because
            // the running_tasks table is workspace-global.
            store.record_background_start(
                &root,
                &session,
                9,
                "delegate work",
                None,
                Some("sub-123"),
            );
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

            // Completing the task through the normal by-session lifecycle
            // clears the row and removes its display label.
            store.clear_background_task(&root, &session, 9);
            for _ in 0..1000 {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                sub_store
                    .label_for_subagent(&root, "sub-123")
                    .await
                    .expect("label after clear"),
                None
            );
            let labels = store
                .peek_unfinished_background(&root, &session)
                .await
                .expect("by-session take after clear");
            assert!(labels.is_empty());
        }

        /// End-to-end background-task recovery across a restart: a task
        /// recorded by store A (fire-and-forget spawn) is visible to a
        /// freshly reopened store B (the "killed on exit" report path),
        /// and after a clear on B a third reopen sees nothing left.
        #[tokio::test]
        async fn sqlite_e2e_running_tasks_recovered_after_reopen() {
            let (_dir, path) = temp_db();
            let root = std::env::temp_dir();
            let session = format!("test-e2e-bg-{}", crate::session::new_id());

            // Store A records a task, then is dropped (process dies before
            // the completion arrives).
            {
                let store = SessionStore::connect(&backend(&path), &root, &session)
                    .await
                    .expect("connect bg store A");
                store.record_background_start(&root, &session, 42, "long build", None, None);
                // Drop before the task completes — simulated crash.
            }

            // Reopened store B recovers the unfinished task (consuming it).
            let store_b = SessionStore::connect(&backend(&path), &root, &session)
                .await
                .expect("reconnect bg store B");
            let recovered = take_unfinished_with_retry(&store_b, &root, &session).await;
            assert_eq!(
                recovered,
                vec![crate::session::format_unfinished(42, "long build", None)],
                "unfinished task must survive the restart"
            );

            // B records and clears a task; a third reopen (another restart)
            // must report nothing.
            store_b.record_background_start(&root, &session, 43, "run tests", None, None);
            for _ in 0..1000 {
                tokio::task::yield_now().await;
            }
            store_b.clear_background_task(&root, &session, 43);
            for _ in 0..1000 {
                tokio::task::yield_now().await;
            }
            let store_c = SessionStore::connect(&backend(&path), &root, &session)
                .await
                .expect("reconnect bg store C");
            let labels = store_c
                .peek_unfinished_background(&root, &session)
                .await
                .expect("take after clear+restart");
            assert!(
                labels.is_empty(),
                "cleared task must not reappear after restart: {labels:?}"
            );
        }
    }

    #[tokio::test]
    async fn jsonl_located_pages_keep_absolute_ordinals_beyond_200() {
        use crate::agent::Message;
        use crate::session::Session;

        let root = std::env::temp_dir();
        let name = format!("test-jsonl-located-pages-{}", crate::session::new_id());
        let entries: Vec<SessionEntry> = (0..205)
            .map(|i| SessionEntry::Message {
                message: Message::User {
                    content: format!("m{i}"),
                    images: vec![],
                },
            })
            .collect();
        Session::append(&root, &name, &entries).unwrap();
        let store = SessionStore::Jsonl;

        let mut page = store
            .load_head_page_located(&root, &name, Some(200))
            .await
            .unwrap();
        assert_eq!(page.entries, entries[5..]);
        assert_eq!(page.next_before_seq, Some(5));
        for (i, location) in page.locations.iter().enumerate() {
            assert!(matches!(
                location.as_ref().map(|location| &location.key),
                Some(LocatedKey::Jsonl { ordinal }) if *ordinal == (i + 5) as i64
            ));
        }

        let older = store
            .load_older_located(&root, &name, page.next_before_seq.unwrap(), Some(200))
            .await
            .unwrap();
        assert_eq!(older.entries, entries[..5]);
        assert_eq!(older.next_before_seq, None);
        page.entries.splice(0..0, older.entries);
        assert_eq!(
            page.entries, entries,
            "located pages cover the file exactly once"
        );
    }

    #[tokio::test]
    async fn legacy_jsonl_located_pages_use_positional_cursor_without_locations() {
        use crate::agent::Message;

        let root = tempfile::tempdir().unwrap();
        let name = "legacy-located";
        let messages = vec![
            Message::User {
                content: "one".into(),
                images: vec![],
            },
            Message::User {
                content: "two".into(),
                images: vec![],
            },
            Message::User {
                content: "three".into(),
                images: vec![],
            },
        ];
        let dir = root.path().join(".e-agent/sessions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{name}.json")),
            serde_json::json!({ "version": 1, "messages": messages }).to_string(),
        )
        .unwrap();
        let store = SessionStore::Jsonl;

        let head = store
            .load_head_page_located(root.path(), name, Some(2))
            .await
            .unwrap();
        assert_eq!(head.entries.len(), 2);
        assert_eq!(head.next_before_seq, Some(1));
        assert!(head.locations.iter().all(Option::is_none));
        let older = store
            .load_older_located(root.path(), name, 1, Some(2))
            .await
            .unwrap();
        assert_eq!(older.entries.len(), 1);
        assert_eq!(older.next_before_seq, None);
        assert!(older.locations.iter().all(Option::is_none));
    }
    // ── Zero-copy fork: marker-only storage + read-through load ──────

    fn fork_user(text: &str) -> SessionEntry {
        SessionEntry::Message {
            message: crate::agent::Message::User {
                content: text.to_owned(),
                images: vec![],
            },
        }
    }

    fn fork_assistant(text: &str) -> SessionEntry {
        SessionEntry::Message {
            message: crate::agent::Message::Assistant(crate::agent::AssistantMessage {
                content: Some(text.to_owned()),
                tool_calls: vec![],
                reasoning: None,
            }),
        }
    }

    /// `turns` completed turns (`user`, `assistant`) — every assistant is a
    /// fork boundary.
    fn fork_turns(turns: usize) -> Vec<SessionEntry> {
        (0..turns)
            .flat_map(|i| {
                [
                    fork_user(&format!("q{i}")),
                    fork_assistant(&format!("a{i}")),
                ]
            })
            .collect()
    }

    /// A LEGACY positional marker: `at` is the inherited entry count.
    fn marker(source: &str, at: usize, seq: Option<i64>) -> SessionEntry {
        SessionEntry::ForkedFrom {
            source: source.to_owned(),
            at: Some(at),
            event_time: None,
            seq,
        }
    }

    /// The LEADING-marker probe (`leading_fork_marker`) stops at its bound
    /// instead of parsing the whole JSONL transcript.
    #[test]
    fn jsonl_load_first_reads_a_bounded_head() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let entries = fork_turns(10);
        crate::session::Session::append(root, "bounded", &entries).unwrap();
        let first = crate::session::Session::load_first(root, "bounded", 3).unwrap();
        assert_eq!(first, entries[..3]);
    }

    /// The copy-based writer this repo shipped before `1edc53d` appended the
    /// marker AFTER the whole selected prefix, so four ordinary turns put it
    /// ninth. Provenance must find it there; a bounded head of eight answered
    /// "not a fork", and the reopened fork regained `link_ref`.
    #[tokio::test]
    async fn jsonl_provenance_finds_a_copy_marker_behind_four_turns() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let prefix = fork_turns(4); // eight entries
        crate::session::Session::append(root, "nine-parent", &prefix).unwrap();
        let mut child = prefix.clone();
        child.push(marker("nine-parent", prefix.len(), Some(7)));
        crate::session::Session::append(root, "nine-fork", &child).unwrap();

        assert!(
            SessionStore::Jsonl
                .has_fork_provenance(root, "nine-fork")
                .await
                .unwrap(),
            "the ninth-entry marker is provenance, not a head-bound miss"
        );
    }

    /// An ordinary long session is not a fork and still builds. JSONL has no
    /// kind index, so the negative answer is one sequential pass; the scan
    /// stops at the first marker (see the test below) and never parses a
    /// legacy `.json` document.
    #[tokio::test]
    async fn jsonl_provenance_is_false_for_a_plain_long_session() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        crate::session::Session::append(root, "plain-long", &fork_turns(500)).unwrap();
        assert!(
            !SessionStore::Jsonl
                .has_fork_provenance(root, "plain-long")
                .await
                .unwrap()
        );
    }

    /// The marker scan stops at the first match: a corrupt line AFTER it is
    /// never decoded by the probe (the loader reports it on a real load).
    #[tokio::test]
    async fn jsonl_provenance_stops_reading_at_the_first_marker() {
        use std::io::Write as _;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let prefix = fork_turns(4);
        crate::session::Session::append(root, "stop-parent", &prefix).unwrap();
        let mut child = prefix;
        child.push(marker("stop-parent", 8, Some(7)));
        crate::session::Session::append(root, "stop-fork", &child).unwrap();
        let path = root.join(".e-agent/sessions/stop-fork.jsonl");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "not json").unwrap();

        assert!(
            SessionStore::Jsonl
                .has_fork_provenance(root, "stop-fork")
                .await
                .unwrap()
        );
    }

    /// A persisted parent link answers "is this a fork" without touching the
    /// transcript: the stored log here is not even valid JSON.
    #[tokio::test]
    async fn persisted_parent_link_answers_without_the_transcript() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        SessionStore::Jsonl
            .create_meta(
                root,
                "meta-child",
                Some("m"),
                Some("subagent"),
                Some("meta-parent"),
                None,
                None,
            )
            .await
            .unwrap();
        std::fs::write(
            root.join(".e-agent/sessions/meta-child.jsonl"),
            "not json\n",
        )
        .unwrap();

        assert!(
            SessionStore::Jsonl
                .has_fork_provenance(root, "meta-child")
                .await
                .unwrap()
        );
    }

    /// An unreadable metadata sidecar is NOT "no persisted parent": an
    /// unknown provenance must never resolve to "ordinary session" and
    /// re-enable `link_ref`. Only `NotFound` is absence.
    #[tokio::test]
    async fn an_unreadable_metadata_sidecar_is_an_error_not_an_absent_parent() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        crate::session::Session::append(root, "meta-blocked", &fork_turns(1)).unwrap();
        // A directory at the sidecar path is openable but not readable on
        // Unix, and refused at open on Windows: both are non-NotFound.
        std::fs::create_dir(root.join(".e-agent/sessions/meta-blocked.meta.jsonl")).unwrap();

        let error = SessionStore::Jsonl
            .has_fork_provenance(root, "meta-blocked")
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("meta-blocked.meta.jsonl"),
            "the sidecar read error is surfaced by name: {error:#}"
        );
    }

    /// The transcript probe follows the same rule: a failed open/read is an
    /// error, never "no marker". A directory at the transcript path fails on
    /// every platform (open or first read).
    #[tokio::test]
    async fn an_unreadable_transcript_is_an_error_not_an_absent_marker() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join(".e-agent/sessions/tr-blocked.jsonl")).unwrap();

        let error = SessionStore::Jsonl
            .has_fork_provenance(root, "tr-blocked")
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("tr-blocked.jsonl"),
            "the transcript read error is surfaced by name: {error:#}"
        );
    }

    /// A legacy whole-document `.json` session stores `Message`s only, so it
    /// can never carry a marker — the probe must not read (or fail on) it.
    #[tokio::test]
    async fn jsonl_provenance_never_parses_a_legacy_document() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let directory = root.join(".e-agent/sessions");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("legacy-only.json"), "{ this is not json").unwrap();

        assert!(
            !SessionStore::Jsonl
                .has_fork_provenance(root, "legacy-only")
                .await
                .unwrap()
        );
    }

    /// Only the two LEADING layouts are a zero-copy fork. A marker deeper in
    /// the log is the copy-based layout older builds wrote, whose prefix
    /// really is stored (resolving it again would duplicate the history).
    #[test]
    fn zero_copy_marker_recognizes_only_the_leading_layouts() {
        let goal = SessionEntry::GoalUpdated { goal: None };
        let fork = marker("src", 3, None);
        assert_eq!(zero_copy_marker(std::slice::from_ref(&fork)), Some(0));
        assert_eq!(zero_copy_marker(&[goal.clone(), fork.clone()]), Some(1));
        assert_eq!(zero_copy_marker(&[fork_user("q"), fork]), None);
        assert_eq!(zero_copy_marker(&[]), None);
        assert_eq!(zero_copy_marker(&[goal]), None);
    }

    fn child_lines(root: &Path, name: &str) -> usize {
        std::fs::read_to_string(root.join(format!(".e-agent/sessions/{name}.jsonl")))
            .map(|raw| raw.lines().filter(|line| !line.trim().is_empty()).count())
            .unwrap_or(0)
    }

    /// The core contract: a fork child's storage holds ONLY the marker (plus
    /// its own later entries). The parent prefix is never copied, and loading
    /// reconstructs it read-only from the source session.
    #[tokio::test]
    async fn zero_copy_fork_child_stores_only_the_marker_and_loads_the_prefix() {
        use crate::session::Session;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let parent = "fork-parent";
        let entries = fork_turns(3);
        Session::append(root, parent, &entries).unwrap();

        let at = 4usize;
        Session::append(
            root,
            "forked",
            std::slice::from_ref(&marker(parent, at, Some(3))),
        )
        .unwrap();
        assert_eq!(
            child_lines(root, "forked"),
            1,
            "the child store holds the marker alone"
        );

        let store = SessionStore::Jsonl;
        let resolved = store.load_fork_located(root, "forked").await.unwrap();
        assert_eq!(resolved.inherited, at);
        assert_eq!(resolved.entries.len(), at + 1);
        assert_eq!(resolved.entries[..at], entries[..at]);
        assert_eq!(resolved.entries[at], marker(parent, at, Some(3)));
        assert!(
            resolved.locations[..at].iter().all(Option::is_none),
            "inherited entries never carry a child location"
        );
        assert!(
            resolved.locations[at].is_some(),
            "the child's own marker is located"
        );

        // The child's own new entries land AFTER the marker (inherited
        // prefix … forked-from … new work) without touching the parent rows.
        Session::append(root, "forked", &[fork_user("own q")]).unwrap();
        assert_eq!(child_lines(root, "forked"), 2);
        let resolved = store.load_fork_located(root, "forked").await.unwrap();
        assert_eq!(resolved.entries.len(), at + 2);
        assert_eq!(resolved.entries[..at], entries[..at]);
        assert_eq!(resolved.entries[at + 1], fork_user("own q"));
        assert!(resolved.locations[at + 1].is_some());

        // A non-fork session is untouched by the marker-aware path.
        let plain = store.load_fork_located(root, parent).await.unwrap();
        assert_eq!(plain.inherited, 0);
        assert_eq!(plain.entries, entries);
    }

    /// The parent is frozen at the marker: appends AFTER the fork never show
    /// up in the child's resolved transcript.
    #[tokio::test]
    async fn zero_copy_fork_child_is_immune_to_later_parent_appends() {
        use crate::session::Session;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let parent = "frozen-parent";
        let entries = fork_turns(2);
        Session::append(root, parent, &entries).unwrap();
        let at = entries.len();
        Session::append(
            root,
            "frozen-fork",
            std::slice::from_ref(&marker(parent, at, Some(at as i64 - 1))),
        )
        .unwrap();

        Session::append(root, parent, &fork_turns(1)).unwrap();
        let store = SessionStore::Jsonl;
        let resolved = store.load_fork_located(root, "frozen-fork").await.unwrap();
        assert_eq!(resolved.entries[..at], entries[..]);
        assert_eq!(
            resolved.entries.len(),
            at + 1,
            "the later parent turn stays invisible: {:?}",
            resolved.entries
        );
    }

    /// A deleted/unreadable source fails closed with a message that names it
    /// instead of silently loading a truncated transcript.
    #[tokio::test]
    async fn zero_copy_fork_child_fails_closed_when_the_source_is_gone() {
        use crate::session::Session;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let entries = fork_turns(2);
        Session::append(root, "gone-parent", &entries).unwrap();
        Session::append(
            root,
            "orphan-fork",
            std::slice::from_ref(&marker("gone-parent", 4, Some(3))),
        )
        .unwrap();
        std::fs::remove_file(root.join(".e-agent/sessions/gone-parent.jsonl")).unwrap();

        let error = SessionStore::Jsonl
            .load_fork_located(root, "orphan-fork")
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("gone-parent"), "{message}");
        assert!(message.contains("missing or empty"), "{message}");
    }

    /// A rewritten/moved source no longer matches the marker's pinned `seq`
    /// and the load fails closed rather than inheriting a different prefix.
    #[tokio::test]
    async fn zero_copy_fork_child_fails_closed_on_a_moved_fork_point() {
        use crate::session::Session;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let entries = fork_turns(2);
        Session::append(root, "moved-parent", &entries).unwrap();
        Session::append(
            root,
            "pinned-fork",
            std::slice::from_ref(&marker("moved-parent", 4, Some(99))),
        )
        .unwrap();

        let error = SessionStore::Jsonl
            .load_fork_located(root, "pinned-fork")
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("moved-parent"), "{message}");
        assert!(
            message.contains("no longer matches the marker"),
            "{message}"
        );
    }

    /// A fork of a fork resolves the whole chain (each level read-only), and
    /// a cyclic ancestry walk fails closed instead of recursing forever.
    #[tokio::test]
    async fn fork_of_fork_resolves_the_chain_and_rejects_cycles() {
        use crate::session::Session;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let grand = fork_turns(2);
        Session::append(root, "grand", &grand).unwrap();
        // middle inherits all 4 grand entries, then adds one of its own.
        Session::append(
            root,
            "middle",
            &[
                marker("grand", 4, Some(3)),
                fork_user("middle q"),
                fork_assistant("middle a"),
            ],
        )
        .unwrap();
        // child inherits the first 6 entries of middle's resolved
        // transcript: grand's 4 entries + middle's marker + "middle q".
        // `seq = 1` pins middle's OWN ordinal 1 ("middle q").
        Session::append(
            root,
            "leaf",
            std::slice::from_ref(&marker("middle", 6, Some(1))),
        )
        .unwrap();

        let resolved = SessionStore::Jsonl
            .load_fork_located(root, "leaf")
            .await
            .unwrap();
        assert_eq!(resolved.inherited, 6);
        assert_eq!(resolved.entries[..4], grand[..]);
        assert_eq!(resolved.entries[4], marker("grand", 4, Some(3)));
        assert_eq!(resolved.entries[5], fork_user("middle q"));
        assert_eq!(resolved.entries[6], marker("middle", 6, Some(1)));
        assert_eq!(resolved.entries.len(), 7);
        assert_eq!(
            SessionStore::Jsonl
                .load_fork_located(root, "middle")
                .await
                .unwrap()
                .inherited,
            4
        );

        // Cycle: two sessions naming each other as source.
        Session::append(
            root,
            "cycle-a",
            std::slice::from_ref(&marker("cycle-b", 1, None)),
        )
        .unwrap();
        Session::append(
            root,
            "cycle-b",
            std::slice::from_ref(&marker("cycle-a", 1, None)),
        )
        .unwrap();
        let error = SessionStore::Jsonl
            .load_fork_located(root, "cycle-a")
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("cyclic"), "{message}");
    }

    /// The copy-based fork layout written by older builds (marker deeper in
    /// the log, parent rows really stored) loads exactly as before.
    #[tokio::test]
    async fn legacy_copy_based_fork_children_load_unchanged() {
        use crate::session::Session;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let entries = fork_turns(1);
        Session::append(root, "legacy-parent", &entries).unwrap();
        let stored = vec![
            entries[0].clone(),
            entries[1].clone(),
            marker("legacy-parent", 2, Some(1)),
            fork_user("own q"),
        ];
        Session::append(root, "legacy-fork", &stored).unwrap();

        let resolved = SessionStore::Jsonl
            .load_fork_located(root, "legacy-fork")
            .await
            .unwrap();
        assert_eq!(resolved.inherited, 0, "the prefix really is stored here");
        assert_eq!(resolved.entries, stored);
        assert!(resolved.locations.iter().all(Option::is_some));
    }

    /// A marker-first `[ForkedFrom] + prefix` log is byte-identical to a
    /// genuine zero-copy child whose own first messages repeat the parent
    /// prefix (`0e32549`'s copy layout had `event_time: None` and no goal
    /// freeze). With no provenance to separate them the load must fail closed
    /// instead of silently dropping the inherited prefix (legacy reading) or
    /// duplicating it (zero-copy reading).
    #[tokio::test]
    async fn marker_first_prefix_is_ambiguous_and_fails_closed() {
        use crate::session::Session;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let entries = fork_turns(2);
        Session::append(root, "lead-parent", &entries).unwrap();
        // Same bytes for both readings: the legacy `0e32549` copy AND a
        // zero-copy child whose own first two messages equal the parent's.
        let stored = vec![
            marker("lead-parent", 4, Some(3)),
            entries[0].clone(),
            entries[1].clone(),
            entries[2].clone(),
            entries[3].clone(),
        ];
        Session::append(root, "lead-fork", &stored).unwrap();

        let error = SessionStore::Jsonl
            .load_fork_located(root, "lead-fork")
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("ambiguous"), "{message}");
        assert!(message.contains("lead-parent"), "{message}");
        assert!(
            message.contains("legacy marker-first fork copy"),
            "{message}"
        );
    }

    /// Provenance breaks the tie: the old marker-first copy never preceded the
    /// marker with the child's frozen `GoalUpdated`, so that shape IS a
    /// zero-copy child (its repeated first messages stay its own).
    #[tokio::test]
    async fn marker_after_a_goal_freeze_is_unambiguously_zero_copy() {
        use crate::session::Session;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let entries = fork_turns(2);
        Session::append(root, "goal-parent", &entries).unwrap();
        let stored = vec![
            SessionEntry::GoalUpdated { goal: None },
            marker("goal-parent", 4, Some(3)),
            entries[0].clone(),
            entries[1].clone(),
        ];
        Session::append(root, "goal-fork", &stored).unwrap();

        let resolved = SessionStore::Jsonl
            .load_fork_located(root, "goal-fork")
            .await
            .unwrap();
        assert_eq!(resolved.inherited, 4);
        assert_eq!(resolved.entries[..4], entries[..]);
        assert_eq!(resolved.entries[4], stored[0]);
        assert_eq!(resolved.entries[5], stored[1]);
        assert_eq!(
            resolved.entries[6..],
            stored[2..],
            "the child's own repeated messages stay after the marker"
        );
    }

    /// The copy-based `[prefix..][ForkedFrom]` layout has no leading marker,
    /// so it keeps loading exactly as before.
    #[tokio::test]
    async fn copy_based_fork_with_a_trailing_marker_loads_unchanged() {
        use crate::session::Session;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let entries = fork_turns(2);
        Session::append(root, "tail-parent", &entries).unwrap();
        let mut tail_layout = entries[..4].to_vec();
        tail_layout.push(marker("tail-parent", 4, Some(3)));
        Session::append(root, "tail-fork", &tail_layout).unwrap();
        let tail = SessionStore::Jsonl
            .load_fork_located(root, "tail-fork")
            .await
            .unwrap();
        assert_eq!(tail.inherited, 0);
        assert_eq!(tail.entries, tail_layout);
        assert!(tail.locations.iter().all(Option::is_some));
    }

    /// A legacy whole-document (`.json`) parent must not make the zero-copy
    /// child look legacy: `legacy` describes the child's OWN store, and the
    /// runner would otherwise rewrite the marker-only log into a full copy.
    #[tokio::test]
    async fn fork_of_a_legacy_json_parent_keeps_a_marker_only_child_store() {
        use crate::agent::Message;
        use crate::session::Session;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let dir = root.join(".e-agent/sessions");
        std::fs::create_dir_all(&dir).unwrap();
        let messages: Vec<Message> = fork_turns(2)
            .into_iter()
            .map(|entry| match entry {
                SessionEntry::Message { message } => message,
                other => panic!("unexpected entry: {other:?}"),
            })
            .collect();
        std::fs::write(
            dir.join("legacy-parent.json"),
            serde_json::json!({ "version": 1, "messages": messages }).to_string(),
        )
        .unwrap();
        let source = Session::load(root, "legacy-parent").unwrap();
        assert!(source.legacy);
        Session::append(
            root,
            "legacy-fork",
            std::slice::from_ref(&marker("legacy-parent", 4, None)),
        )
        .unwrap();

        let resolved = SessionStore::Jsonl
            .load_fork_located(root, "legacy-fork")
            .await
            .unwrap();
        assert_eq!(resolved.inherited, 4);
        assert_eq!(resolved.entries[..4], source.entries[..]);
        assert!(
            !resolved.legacy,
            "the child's own JSONL store is not legacy — it must not be rewritten"
        );
        assert_eq!(
            child_lines(root, "legacy-fork"),
            1,
            "the child store stays marker-only"
        );
    }

    /// The fork-boundary scans index the RESOLVED transcript, so `at` means
    /// the same thing to the panel and to the fork itself.
    #[tokio::test]
    async fn load_fork_with_seq_indexes_the_resolved_transcript() {
        use crate::session::Session;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let entries = fork_turns(2);
        Session::append(root, "seq-parent", &entries).unwrap();
        Session::append(
            root,
            "seq-fork",
            &[marker("seq-parent", 4, Some(3)), fork_user("own q")],
        )
        .unwrap();

        let store = SessionStore::Jsonl;
        let with_seq = store.load_fork_with_seq(root, "seq-fork").await.unwrap();
        assert_eq!(with_seq.len(), 6);
        assert_eq!(
            with_seq[..4],
            [
                (0, entries[0].clone()),
                (1, entries[1].clone()),
                (2, entries[2].clone()),
                (3, entries[3].clone()),
            ]
        );
        assert_eq!(with_seq[4].1, marker("seq-parent", 4, Some(3)));
        // The child's own second line carries its own ordinal (1).
        assert_eq!(with_seq[5], (1, fork_user("own q")));

        // The Web head page resolves the inherited prefix as one page.
        let page = store
            .load_head_page_located(root, "seq-fork", Some(2))
            .await
            .unwrap();
        assert_eq!(page.entries.len(), 6, "no physical cursor to page by");
        assert_eq!(page.next_before_seq, None);
        assert!(page.locations[..4].iter().all(Option::is_none));
    }

    // ── Pinned (owner + key) fork markers ────────────────────────────

    /// A NEW-generation marker: no positional `at`; the cut row is pinned by
    /// the session that physically stores it plus that row's backend key.
    fn pinned(source: &str, seq: i64, event_time: Option<i64>) -> SessionEntry {
        SessionEntry::ForkedFrom {
            source: source.to_owned(),
            at: None,
            event_time,
            seq: Some(seq),
        }
    }

    fn compaction(summary: &str) -> SessionEntry {
        SessionEntry::Compaction {
            summary: summary.to_owned(),
            retained: Vec::new(),
            current_prompt_at: None,
            no_current_prompt: false,
        }
    }

    /// The pinned cut inherits from the LAST COMPACTION AT OR BEFORE it — not
    /// from the owner's latest compaction overall and never from a later one.
    #[tokio::test]
    async fn pinned_marker_inherits_from_the_last_compaction_at_or_before_the_cut() {
        use crate::session::Session;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        // JSONL ordinals: 0 q0, 1 a0, 2 q1, 3 a1, 4 compaction, 5 q2, 6 a2.
        let rows = vec![
            fork_user("q0"),
            fork_assistant("a0"),
            fork_user("q1"),
            fork_assistant("a1"),
            compaction("sum 0..4"),
            fork_user("q2"),
            fork_assistant("a2"),
        ];
        Session::append(root, "pc-parent", &rows).unwrap();
        let store = SessionStore::Jsonl;

        // AFTER the compaction (ordinal 6): the context starts AT it.
        Session::append(root, "after", &[pinned("pc-parent", 6, None)]).unwrap();
        let resolved = store.load_fork_located(root, "after").await.unwrap();
        assert_eq!(resolved.inherited, 3);
        assert_eq!(resolved.entries[..3], rows[4..]);
        assert_eq!(resolved.entries[3], pinned("pc-parent", 6, None));

        // EXACTLY AT the compaction (ordinal 4): the compaction alone.
        Session::append(root, "at-comp", &[pinned("pc-parent", 4, None)]).unwrap();
        let resolved = store.load_fork_located(root, "at-comp").await.unwrap();
        assert_eq!(resolved.inherited, 1);
        assert_eq!(resolved.entries[0], rows[4]);

        // BEFORE it (ordinal 3): no compaction applies, so the owner's own
        // first rows are the context.
        Session::append(root, "before", &[pinned("pc-parent", 3, None)]).unwrap();
        let resolved = store.load_fork_located(root, "before").await.unwrap();
        assert_eq!(resolved.inherited, 4);
        assert_eq!(resolved.entries[..4], rows[..4]);
        assert!(
            resolved.locations[..4].iter().all(Option::is_none),
            "inherited entries carry no child location"
        );
    }

    /// A pinned fork is frozen at the row it named: later appends — a new
    /// compaction included — never move its context, and rows after the cut
    /// are never inherited.
    #[tokio::test]
    async fn pinned_marker_is_frozen_at_its_cut_across_later_source_compactions() {
        use crate::session::Session;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let rows = fork_turns(2);
        Session::append(root, "frozen-pc-parent", &rows).unwrap();
        Session::append(
            root,
            "frozen-pc-fork",
            &[pinned("frozen-pc-parent", 1, None)],
        )
        .unwrap();
        let store = SessionStore::Jsonl;
        let before = store
            .load_fork_located(root, "frozen-pc-fork")
            .await
            .unwrap();
        assert_eq!(before.inherited, 2);
        assert_eq!(before.entries[..2], rows[..2]);

        // A later completed turn never appears…
        Session::append(root, "frozen-pc-parent", &fork_turns(1)).unwrap();
        // …and neither does a later compaction, even though it is now the
        // owner's LATEST one.
        Session::append(
            root,
            "frozen-pc-parent",
            &[compaction("sum everything"), fork_user("q9")],
        )
        .unwrap();
        let after = store
            .load_fork_located(root, "frozen-pc-fork")
            .await
            .unwrap();
        assert_eq!(after.entries, before.entries);
        assert_eq!(after.inherited, 2);
    }

    /// A pinned marker with no row to pin fails closed: the owner no longer
    /// carries that `seq`, or the marker never recorded one.
    #[tokio::test]
    async fn pinned_marker_fails_closed_without_its_pinned_row() {
        use crate::session::Session;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        Session::append(root, "pc-gone-parent", &fork_turns(2)).unwrap();
        Session::append(root, "pc-missing", &[pinned("pc-gone-parent", 99, None)]).unwrap();
        Session::append(
            root,
            "pc-unpinned",
            &[SessionEntry::ForkedFrom {
                source: "pc-gone-parent".into(),
                at: None,
                event_time: None,
                seq: None,
            }],
        )
        .unwrap();

        let error = SessionStore::Jsonl
            .load_fork_located(root, "pc-missing")
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("pc-gone-parent"), "{message}");
        assert!(message.contains("no longer contains"), "{message}");

        let error = SessionStore::Jsonl
            .load_fork_located(root, "pc-unpinned")
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("without a pinned seq"), "{message}");
    }

    /// Ownership comes from the selected PHYSICAL ROW, never from the numeric
    /// `seq`: the ancestor's ordinal 1 and the child's ordinal 1 are different
    /// entries, and a cut inside an inherited prefix descends to the ancestor
    /// that really stores it.
    #[tokio::test]
    async fn pinned_cut_ownership_follows_the_row_not_the_seq() {
        use crate::session::Session;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let grand = fork_turns(2); // ordinals 0..3
        Session::append(root, "own-grand", &grand).unwrap();
        // middle: its own ordinal 1 is "middle q", pinning grand's ordinal 3.
        Session::append(
            root,
            "own-middle",
            &[
                pinned("own-grand", 3, None),
                fork_user("middle q"),
                fork_assistant("middle a"),
            ],
        )
        .unwrap();
        // A cut at grand's ordinal 1 lives in GRAND…
        Session::append(root, "own-leaf-g", &[pinned("own-grand", 1, None)]).unwrap();
        // …while middle's OWN ordinal 1 is "middle q".
        Session::append(root, "own-leaf-m", &[pinned("own-middle", 1, None)]).unwrap();
        let store = SessionStore::Jsonl;

        let grand_leaf = store.load_fork_located(root, "own-leaf-g").await.unwrap();
        assert_eq!(grand_leaf.inherited, 2);
        assert_eq!(grand_leaf.entries[..2], grand[..2]);

        let middle_leaf = store.load_fork_located(root, "own-leaf-m").await.unwrap();
        // The cut precedes middle's every compaction, so its context starts at
        // middle's own first stored row — which is middle's marker, whose own
        // prefix is grand's read-through history.
        assert_eq!(middle_leaf.entries[..4], grand[..]);
        assert_eq!(middle_leaf.entries[4], pinned("own-grand", 3, None));
        assert_eq!(middle_leaf.entries[5], fork_user("middle q"));
        assert_eq!(middle_leaf.inherited, 6);

        // A producer resolving the same logical cut gets the physical owner.
        let origin = store.fork_cut_origin(root, "own-middle", 2).await.unwrap();
        assert_eq!(origin.0, "own-grand", "an inherited cut belongs to grand");
        assert_eq!(origin.1.seq(), 1);
        let origin = store.fork_cut_origin(root, "own-middle", 6).await.unwrap();
        assert_eq!(origin.0, "own-middle", "middle's own row stays middle's");
        assert_eq!(origin.1.seq(), 1);
    }

    /// A bounded JSONL head page hands out the ABSOLUTE position of its first
    /// returned row, so a head anchored at an arbitrary compaction ordinal
    /// pages back without skipping anything.
    #[tokio::test]
    async fn jsonl_bounded_head_cursor_is_absolute_and_recovers_every_row() {
        use crate::session::Session;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        // Ordinals 0..99 are summarized rows, 100 is the compaction, 101..109
        // are the head tail.
        let mut rows: Vec<SessionEntry> =
            (0..100).map(|i| fork_user(&format!("old {i}"))).collect();
        rows.push(compaction("sum 0..100"));
        for i in 0..9 {
            rows.push(fork_user(&format!("head {i}")));
        }
        Session::append(root, "abs-cursor", &rows).unwrap();
        let store = SessionStore::Jsonl;

        // Head = ordinals 100..109 (10 rows); limit 2 returns the newest two.
        let page = store
            .load_head_page_located(root, "abs-cursor", Some(2))
            .await
            .unwrap();
        assert_eq!(page.entries, vec![fork_user("head 7"), fork_user("head 8")]);
        assert_eq!(
            page.next_before_seq,
            Some(108),
            "cursor is the first returned row's ABSOLUTE ordinal"
        );

        // Walking only the returned cursors recovers every row exactly once.
        let mut walked: Vec<SessionEntry> = Vec::new();
        let mut cursor = page.next_before_seq;
        let mut guard = 0;
        while let Some(before) = cursor {
            let older = store
                .load_fork_older_located(root, "abs-cursor", before, Some(2))
                .await
                .unwrap();
            let next = older.next_before_seq;
            walked.splice(0..0, older.entries);
            cursor = next;
            guard += 1;
            assert!(guard < 200, "the cursor chain must terminate");
        }
        walked.extend(page.entries);
        let mut expected = rows.clone();
        expected.truncate(110);
        assert_eq!(walked.len(), expected.len(), "no gap, no duplicate");
        assert_eq!(walked, expected, "the whole file, once");

        // No limit: the whole head + its own route, unchanged.
        let whole = store
            .load_head_page_located(root, "abs-cursor", None)
            .await
            .unwrap();
        assert_eq!(whole.entries.len(), 10);
        assert_eq!(whole.next_before_seq, Some(100));
    }

    /// The head page of a compacted pinned fork exposes the OWNER's cursor for
    /// older AUDIT rows: paging recovers the earlier owner entries — with no
    /// duplicates and nothing after the pinned cut — and follows the owner's
    /// own marker into its ancestor.
    #[tokio::test]
    async fn pinned_fork_head_keeps_an_older_audit_route_across_ancestry() {
        use crate::session::Session;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let grand = fork_turns(2); // ordinals 0..3
        Session::append(root, "route-grand", &grand).unwrap();
        // mid: its own ordinal 3 is "q2"; the compaction at ordinal 1 opens
        // the context a pinned child inherits.
        let mid = vec![
            pinned("route-grand", 3, None),
            compaction("mid summary"),
            fork_user("q2"),
            fork_assistant("a2"),
        ];
        Session::append(root, "route-mid", &mid).unwrap();
        Session::append(root, "route-leaf", &[pinned("route-mid", 3, None)]).unwrap();
        let store = SessionStore::Jsonl;

        // Model context: only the compaction segment (never the pre-compaction
        // rows, and never replayed alongside their summary).
        let loaded = store
            .load_fork_head_located(root, "route-leaf")
            .await
            .unwrap();
        assert_eq!(loaded.inherited, 3, "compaction + q2 + a2");
        assert_eq!(loaded.entries[..3], mid[1..]);
        assert_eq!(
            loaded.next_older,
            Some(("route-mid".into(), 1)),
            "older rows live in the owner, below the context"
        );

        // The head PAGE carries that route for the pager.
        let page = store
            .load_head_page_located(root, "route-leaf", Some(2))
            .await
            .unwrap();
        assert_eq!(page.next_before_seq, Some(1));
        assert_eq!(page.next_older_session.as_deref(), Some("route-mid"));

        // Page 1: the owner's rows below the compaction (its own marker).
        let older = store
            .load_fork_older_located(root, "route-mid", 1, None)
            .await
            .unwrap();
        assert_eq!(older.entries, vec![pinned("route-grand", 3, None)]);
        // The owner is itself a fork child: continue in ITS ancestor, and
        // INCLUDE the ancestor's pinned cut row — a compaction summarized it in
        // the child's context, so nothing else materialized it. Rows after the
        // cut stay excluded.
        assert_eq!(older.next_before_seq, Some(4));
        assert_eq!(older.next_older_session.as_deref(), Some("route-grand"));

        // A fork child that compacted on its OWN still continues through its
        // marker at its physical start — even though the cursor's owner is the
        // requested session itself.
        let mid_head = store
            .load_head_page_located(root, "route-mid", None)
            .await
            .unwrap();
        assert_eq!(
            mid_head.entries[0], mid[1],
            "its own head starts at the compaction"
        );
        assert_eq!(mid_head.next_before_seq, Some(1));
        assert_eq!(
            mid_head.next_older_session, None,
            "the owner is the requested session itself"
        );
        let crossed = store
            .load_fork_older_located(root, "route-mid", 1, None)
            .await
            .unwrap();
        assert_eq!(crossed.entries, vec![pinned("route-grand", 3, None)]);
        assert_eq!(crossed.next_older_session.as_deref(), Some("route-grand"));
        assert_eq!(crossed.next_before_seq, Some(4));

        // Page 2: the grand ancestor's rows up to and including its cut.
        let older = store
            .load_fork_older_located(root, "route-grand", 4, None)
            .await
            .unwrap();
        assert_eq!(older.entries, grand[..4].to_vec());
        assert_eq!(older.next_before_seq, None);
        assert_eq!(older.next_older_session, None);

        // Stitching the pages back together yields the complete AUDIT trail
        // once: grand through its cut, the anchor's own marker, then the
        // child's materialized context. Nothing repeats and nothing above the
        // pinned cut appears.
        let mut recovered = Vec::new();
        recovered.extend(older.entries);
        recovered.push(pinned("route-grand", 3, None));
        recovered.extend(loaded.entries[..3].iter().cloned());
        assert_eq!(
            recovered,
            vec![
                fork_user("q0"),
                fork_assistant("a0"),
                fork_user("q1"),
                fork_assistant("a1"),
                pinned("route-grand", 3, None),
                compaction("mid summary"),
                fork_user("q2"),
                fork_assistant("a2"),
            ],
            "the recovered trail must not duplicate or overrun the cut"
        );
    }

    /// A legacy positional marker WITHOUT any pinned key still loads exactly
    /// as before: missing provenance is not a rejection reason.
    #[tokio::test]
    async fn legacy_marker_without_provenance_still_loads_positionally() {
        use crate::session::Session;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let entries = fork_turns(3);
        Session::append(root, "plain-legacy-parent", &entries).unwrap();
        Session::append(
            root,
            "plain-legacy-fork",
            &[marker("plain-legacy-parent", 4, None)],
        )
        .unwrap();
        let resolved = SessionStore::Jsonl
            .load_fork_located(root, "plain-legacy-fork")
            .await
            .unwrap();
        assert_eq!(resolved.inherited, 4);
        assert_eq!(resolved.entries[..4], entries[..4]);
        assert_eq!(resolved.entries[4], marker("plain-legacy-parent", 4, None));
    }
}

#[cfg(test)]
#[path = "session_jsonl_meta_tests.rs"]
mod jsonl_meta_tests;
