//! Runtime session backend dispatch. A simple enum chooses between the
//! default JSONL file backend and the optional GreptimeDB / SQLite backends
//! without introducing a trait (see AGENTS.md: "one adapter, no seam").
//!
//! Each variant holds whatever state its backend needs:
//!
//! - **Jsonl** — stateless marker; every call provides `root` + `name`.
//! - **Greptime** — a connected + session-bound client behind a Mutex so
//!   `&self` methods work everywhere (including the delegate's closure-based
//!   the runner persistence path).
//! - **Sqlite** — a connected + session-bound client behind a Mutex, same
//!   shape as Greptime; the session id is bound at connect time and the
//!   database file is shared across sessions.
//!
//! Background-task state (`*.background.jsonl` files on JSONL, the
//! `running_tasks` table on Greptime/SQLite) is dispatched through the same
//! enum: [`SessionStore::record_background_start`] /
//! [`SessionStore::clear_background_task`] /
//! [`SessionStore::take_unfinished_background`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
#[cfg(any(feature = "greptime", feature = "sqlite"))]
use std::sync::Arc;
#[cfg(any(feature = "greptime", feature = "sqlite"))]
use tokio::sync::Mutex;

use anyhow::{Context, Result};

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::agent::{Message, SessionEntry};
use std::io::{BufRead, Write};

use crate::config::SessionBackend;
use crate::session::{LoadedSession, Session};

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
/// to be set (hence the `COMPUTERNAME` fallback for Windows, then
/// `"unknown"`). The `nonce` disambiguates pid reuse: a simple hash of the
/// boot-time `SystemTime` nanos XORed with the pid — no new dependency,
/// and different processes (or restarts with a reused pid) get different
/// values with overwhelming probability.
static PROCESS_IDENTITY: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The current process's identity string (see [`PROCESS_IDENTITY`]).
pub(crate) fn process_identity() -> &'static str {
    PROCESS_IDENTITY.get_or_init(|| {
        let pid = std::process::id();
        let hostname = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| "unknown".to_owned());
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

/// Whether the process that wrote an `identity` string
/// (`pid@hostname#nonce`, see [`process_identity`]) is still alive.
///
/// Deliberately conservative: any failure to determine liveness reports
/// **true** ("alive"), so callers keep the safe `Preserve` behavior
/// (leave the unfinished-task records for the owning process) instead of
/// wrongly consuming them and injecting a "killed with the process"
/// notice. Only a *definite* dead owner returns false:
///
/// - malformed identity (no `@`/`#`, unparsable pid) → true
/// - hostname differs from the current process's → true (a record from
///   another machine cannot be probed here)
/// - either hostname fell back to `"unknown"` (no HOSTNAME/COMPUTERNAME
///   on one machine) → true: the machines cannot be compared, and probing
///   a foreign pid risks a false "dead" across machines
/// - the probe itself fails (e.g. `kill` missing) → true
/// - unix (Linux and other /proc platforms): `kill -0 <pid>` — exit 0
///   means alive; a non-zero exit is ESRCH (definitely dead) OR EPERM
///   (alive but owned by another user — sudo-launched agent, systemd
///   service, container), so it is disambiguated via the world-readable
///   `/proc/{pid}` directory: present → alive, absent → dead. EPERM can
///   therefore never report dead.
/// - macOS (no /proc): the same two non-zero-exit cases are
///   disambiguated by reading `kill -0`'s stderr, where the kernel names
///   the reason — "Operation not permitted" (EPERM) → alive,
///   "No such process" (ESRCH) → dead, any other message → alive
///   (conservative, see [`classify_kill_stderr`]).
/// - windows: `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid)` —
///   a valid handle means alive (closed right away); NULL with
///   `ERROR_ACCESS_DENIED` means the process exists but belongs to
///   another user → alive, any other NULL error means truly absent.
///
/// The `nonce` is deliberately ignored: liveness is per pid+hostname, and
/// a reused pid answers "some process with that pid is alive", which is
/// exactly the conservative outcome.
pub(crate) fn owner_alive(identity: &str) -> bool {
    let Some((pid_str, rest)) = identity.split_once('@') else {
        return true; // malformed: cannot judge
    };
    let Some((hostname, _nonce)) = rest.split_once('#') else {
        return true; // malformed: cannot judge
    };
    let hostname_now = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".to_owned());
    // Either side falling back to "unknown" makes the two machines
    // incomparable: the record could be from a different machine whose
    // hostname env is also unset, and probing its pid there could report
    // a live process as dead. Sacrifice the same-machine notice in a bare
    // environment to eliminate that cross-machine misreport.
    if hostname == "unknown" || hostname_now == "unknown" {
        return true; // cannot judge: conservative
    }
    if hostname != hostname_now {
        return true; // record from another machine: cannot judge
    }
    let Ok(pid) = pid_str.parse::<u32>() else {
        return true; // unparsable pid: cannot judge
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // No new dependency (no libc): probe via the `kill` command.
        // `kill -0` never signals; it only checks existence + permission.
        // stdout/stderr are silenced: a dead pid makes `kill` print
        // "No such process" to stderr, which would be noise on every
        // server restart that consumes zombie records.
        match std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        {
            Ok(status) if status.success() => true, // alive and signalable
            Ok(_) => {
                // Non-zero exit: ESRCH (no such process — definitely
                // dead) or EPERM (alive but owned by another user).
                // /proc is world-readable on Linux, so its existence is
                // authoritative regardless of permissions: present →
                // alive (EPERM), absent → dead (ESRCH). This closes the
                // one real false-dead path.
                std::path::Path::new(&format!("/proc/{pid}")).exists()
            }
            Err(_) => true, // probe failed: conservative
        }
    }
    #[cfg(target_os = "macos")]
    {
        // macOS has no /proc, so a non-zero exit cannot be disambiguated
        // via the filesystem. `kill -0`'s stderr names the reason,
        // though: "Operation not permitted" (EPERM — the process is
        // alive but owned by another user) vs "No such process" (ESRCH —
        // definitely dead). Read it; anything unclassifiable falls back
        // to conservative alive. stdout stays null (unused).
        match std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
        {
            Ok(out) if out.status.success() => true, // alive and signalable
            Ok(out) => classify_kill_stderr(&String::from_utf8_lossy(&out.stderr)),
            Err(_) => true, // probe failed: conservative
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, GetLastError};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            // ERROR_ACCESS_DENIED: the process exists but belongs to
            // another user (cross-user server attach) — alive, not dead.
            // Any other NULL error means the pid truly does not exist.
            return unsafe { GetLastError() == ERROR_ACCESS_DENIED };
        }
        unsafe {
            CloseHandle(handle);
        }
        true
    }
    #[cfg(not(any(unix, windows)))]
    {
        true
    }
}

/// Classify the stderr of a failed `kill -0` on platforms without /proc
/// (macOS): the kernel names the reason, which disambiguates ESRCH
/// (definitely dead) from EPERM (alive but owned by another user) —
/// the exact EPERM-false-dead path `/proc` closes on Linux. Anything
/// unclassifiable is conservative **alive**, so a weird locale or a
/// future macOS message can never report a live process as dead.
///
/// Compiled on macOS (where `owner_alive` calls it) and in test builds
/// on every platform so the pure classifier stays unit-testable.
#[cfg(any(target_os = "macos", test))]
fn classify_kill_stderr(stderr: &str) -> bool {
    if stderr.contains("Operation not permitted") {
        true // EPERM: the process exists but is owned by another user
    } else if stderr.contains("No such process") {
        false // ESRCH: definitely dead
    } else {
        true // unclassifiable: conservative alive
    }
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

/// Fingerprint of a backend instance for receipt output: hex
/// HMAC-SHA256 (truncated to 32 hex chars) over `backend_kind` + NUL +
/// the normalized instance identity, keyed by the receipt secret — the
/// resolved SQLite database path, the Greptime connection string, or the
/// JSONL workspace root. Receipts carry only this keyed digest (the raw
/// path / connection string never appears in a receipt or an error), and
/// `read_field` rejects a receipt bound to a different backend instance
/// (e.g. another database file or a different GreptimeDB) before
/// querying.
///
/// Keying matters: an UNKEYED hash of a low-entropy connection string
/// (a default SQLite path, a well-known DSN) is an offline verifier —
/// anyone holding a receipt could confirm it came from a guessed instance
/// by hashing the guess. With the receipt secret in the HMAC, the
/// published digest cannot be recomputed without the key.
pub(crate) fn backend_instance_fingerprint(backend_kind: &str, instance: &str) -> String {
    match crate::output_receipt::ReceiptCodec::load() {
        Ok(codec) => keyed_backend_instance_fingerprint(codec.key(), backend_kind, instance),
        Err(_) => {
            // Keyless environment (no state dir / key unavailable): no
            // receipt is ever issued or verified here, so this digest is
            // never published. Keep plain append/load paths deterministic
            // with the historical unkeyed SHA-256.
            let mut identity = String::with_capacity(backend_kind.len() + 1 + instance.len());
            identity.push_str(backend_kind);
            identity.push('\0');
            identity.push_str(instance);
            crate::agent::image_sha256(identity.as_bytes())[..32].to_string()
        }
    }
}

/// The keyed core of [`backend_instance_fingerprint`]: HMAC-SHA256 over
/// `backend_kind` + NUL + `instance` with the receipt key, truncated to
/// 16 bytes (32 hex chars). Exposed for tests: the same instance under
/// two different keys must yield two different fingerprints.
pub(crate) fn keyed_backend_instance_fingerprint(
    key: &[u8; 32],
    backend_kind: &str,
    instance: &str,
) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(backend_kind.as_bytes());
    mac.update(&[0]);
    mac.update(instance.as_bytes());
    let tag = mac.finalize().into_bytes();
    hex_encode_truncated(&tag)
}

/// Lowercase hex of the first 16 bytes (32 hex chars) — the truncation
/// applied to every fingerprint published in a receipt.
fn hex_encode_truncated(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(32);
    for byte in bytes.iter().take(16) {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
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
#[allow(dead_code)]
const HEAD_OPEN_SENTINEL: i64 = i64::MAX;

/// The exact physical key of one persisted `session_entries` row/line, as
/// stored by the owning backend. Pinned (never "latest wins"): a receipt
/// issued against this key reads exactly this physical version, so a
/// same-seq later write can never retarget an old ref (the entry hash check
/// in `read_field` additionally rejects any payload drift).
#[derive(Clone, Debug, PartialEq, Eq)]
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
}

/// The exact physical location of one persisted entry: backend kind
/// fingerprint, backend-instance fingerprint, session id, the backend's
/// pinned key, and the SHA-256 hex of the exact persisted payload bytes
/// (the serialized `SessionEntry` line/row). Aligned 1:1 with the agent
/// history by [`crate::agent::Agent::restore_locations`]; the provider
/// projection (`src/output_receipt.rs`) turns a location into a
/// MAC-protected `eout1` receipt, and `read_output` resolves the receipt
/// back to this location.
#[derive(Clone, Debug, PartialEq, Eq)]
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

#[derive(Clone)]
pub enum SessionStore {
    /// Default file-based JSONL backend. Stateless — delegates to
    /// `session::Session` static methods.
    Jsonl,
    /// GreptimeDB-backed session storage behind a mutex so `&self` methods
    /// work from any context.
    #[cfg(feature = "greptime")]
    Greptime {
        /// The connected Greptime session client, session-bound.
        session: Arc<Mutex<crate::session_greptime::GreptimeSession>>,
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
    pub label: Option<String>,
    pub started_at_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    /// "completed" | "failed" | "killed"
    pub status: Option<String>,
    /// "bash" | "delegate"
    pub kind: Option<String>,
}

/// Where a live agent records its in-flight background tasks: the workspace
/// root, the session name the record belongs to, and the store that owns
/// the record. Carried by the main agent (`agent.background_record`) and
/// the delegate tool (`delegate.record_in`) so background bash and delegate
/// tasks are reported as "killed on exit" when their session is resumed.
#[derive(Clone)]
pub struct BackgroundRecord {
    pub root: PathBuf,
    pub session: String,
    pub store: SessionStore,
}

impl SessionStore {
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
                    session: Arc::new(Mutex::new(session)),
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
                    session: Arc::new(Mutex::new(session)),
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
                let entries = { session.lock().await.load().await? };
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
            SessionStore::Greptime { session, .. } => Ok(session.lock().await.entry_count()),
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
                let entries = { session.lock().await.load_head().await? };
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
                session
                    .lock()
                    .await
                    .load_older(HEAD_OPEN_SENTINEL, limit)
                    .await
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
            SessionStore::Greptime { session, .. } => {
                session.lock().await.load_older(before_seq, limit).await
            }
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
            SessionStore::Greptime { session, .. } => session.lock().await.load_oldest().await,
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
            SessionStore::Greptime { session, .. } => {
                session.lock().await.load_newer(_after_seq).await
            }
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
            SessionStore::Greptime { session, .. } => session.lock().await.head_seq().await,
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
    /// For JSONL there is no seq column; the 0-based ordinal (the JSONL line
    /// index) is returned so the provenance is still meaningful. For
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
            SessionStore::Greptime { session, .. } => session.lock().await.load_with_seq().await,
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .load_with_seq()
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
            SessionStore::Greptime { session, .. } => session.lock().await.append(entries).await,
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
    /// resolves). JSONL ordinals are the appended lines' 0-based positions
    /// (the file is line-counted before appending); Greptime/SQLite return
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
            SessionStore::Greptime { session, .. } => {
                session.lock().await.append_located(entries).await
            }
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
                let rows = session.lock().await.load_located().await?;
                Ok(LoadedLocated {
                    entries: rows.iter().map(|(_, entry)| entry.clone()).collect(),
                    locations: rows.into_iter().map(|(loc, _)| Some(loc)).collect(),
                    legacy: false,
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
                })
            }
        }
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
            SessionStore::Greptime { session, .. } => {
                session.lock().await.read_field(verified).await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .read_field(verified)
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
        usage: &crate::agent::Usage,
    ) -> Result<()> {
        match self {
            SessionStore::Jsonl => Ok(()),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                let workspace_id = derive_workspace_id(root);
                session
                    .lock()
                    .await
                    .append_usage(&workspace_id, session_id, model, kind, usage)
                    .await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => {
                let workspace_id = derive_workspace_id(root);
                session
                    .lock()
                    .await
                    .append_usage(&workspace_id, session_id, model, kind, usage)
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
                session
                    .lock()
                    .await
                    .finished_tasks(&workspace_id, limit)
                    .await
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
            SessionStore::Greptime { session, .. } => session.lock().await.usage_summary().await,
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
            SessionStore::Greptime { session, .. } => {
                session.lock().await.usage_for_sessions(session_ids).await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .usage_for_sessions(session_ids)
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
    /// I/O is small; matching the sync facade of
    /// `record_background_start`). Never self-creates on any backend
    /// (R3): a session with no metadata row / sidecar is a no-op.
    pub fn touch_meta(&self, root: &Path, session: &str) {
        match self {
            SessionStore::Jsonl => {
                if let Err(error) = jsonl_touch_meta(root, session) {
                    eprintln!("e-agent: cannot touch session metadata: {error:#}");
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
                            if let Err(error) = greptime.lock().await.touch_meta().await {
                                eprintln!("e-agent: cannot touch session metadata: {error:#}");
                            }
                        });
                    }
                    Err(_) => {
                        eprintln!("e-agent: cannot touch session metadata: no tokio runtime");
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
                                eprintln!("e-agent: cannot touch session metadata: {error:#}");
                            }
                        });
                    }
                    Err(_) => {
                        eprintln!("e-agent: cannot touch session metadata: no tokio runtime");
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

    /// List the latest metadata snapshot per session (Greptime/SQLite:
    /// newest activity first from the audit table; JSONL: one tail-read of
    /// each transcript's sidecar), newest activity first.
    pub async fn list_meta(&self, root: &Path) -> Result<Vec<SessionMeta>> {
        match self {
            SessionStore::Jsonl => jsonl_list_meta(root),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => session.lock().await.list_meta().await,
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .list_meta()
                .await
                .map_err(anyhow::Error::msg),
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
            } => greptime_session.lock().await.delete_meta(session).await,
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
            } => {
                greptime_session
                    .lock()
                    .await
                    .set_title(session, title)
                    .await
            }
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
            } => {
                greptime_session
                    .lock()
                    .await
                    .set_pinned(session, pinned)
                    .await
            }
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
            } => {
                greptime_session
                    .lock()
                    .await
                    .set_archived(session, archived)
                    .await
            }
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
            SessionStore::Greptime { session, .. } => {
                session.lock().await.backfill_sessions().await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .backfill_sessions()
                .await
                .map_err(anyhow::Error::msg),
        }
    }

    // ------------------------------------------------------------------
    // Background-task state records
    // ------------------------------------------------------------------
    //
    // Sync facade for the record/clear paths: JSONL writes the record file
    // synchronously; Greptime/SQLite schedule the write onto the current
    // tokio runtime (every production call site — tool completion, task
    // ack, delegate cleanup, TUI cancel — runs inside one). Errors are
    // reported on stderr: recording must never break the agent loop,
    // matching the `let _ =` callers of the old
    // `Session::record_background_start`.

    /// Record a freshly started background task so a later launch can tell
    /// the user what died with the previous process.
    ///
    /// `label` 是源头截断的 100 字符预览（「被杀」Notice 用）；`full_command`
    /// 是完整命令原文（bash 任务传 `Some`，delegate 无命令传 `None`），
    /// 持久化后供 UI/`task_full_command` 取用。旧记录缺该字段 → 读回 `None`。
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
                    eprintln!("e-agent: cannot record background task: {error:#}");
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
                                eprintln!("e-agent: cannot record background task: {error:#}");
                            }
                        });
                    }
                    Err(_) => {
                        eprintln!("e-agent: cannot record background task: no tokio runtime");
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
                                eprintln!("e-agent: cannot record background task: {error:#}");
                            }
                        });
                    }
                    Err(_) => {
                        eprintln!("e-agent: cannot record background task: no tokio runtime");
                    }
                }
            }
        }
    }

    /// Forget one task: its completion arrived while the process was alive.
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
                            if let Err(error) =
                                greptime.lock().await.clear_task(&session_id, id).await
                            {
                                eprintln!("e-agent: cannot clear background task: {error:#}");
                            }
                        });
                    }
                    Err(_) => {
                        eprintln!("e-agent: cannot clear background task: no tokio runtime");
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
                                eprintln!("e-agent: cannot clear background task: {error:#}");
                            }
                        });
                    }
                    Err(_) => {
                        eprintln!("e-agent: cannot clear background task: no tokio runtime");
                    }
                }
            }
        }
    }

    /// Tasks recorded by a previous process that died before their
    /// completion arrived. Consumes the record (file or table rows) and
    /// returns the labels so the caller can inject the "killed on exit"
    /// notice. Only this session's own records are returned.
    pub async fn take_unfinished_background(
        &self,
        root: &Path,
        session: &str,
    ) -> Result<Vec<String>> {
        match self {
            SessionStore::Jsonl => Ok(Session::take_unfinished_background(root, session)),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime {
                session: greptime_session,
                ..
            } => {
                let session_id = session.to_owned();
                greptime_session
                    .lock()
                    .await
                    .take_unfinished_tasks(&session_id)
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
                    .take_unfinished_tasks(&session_id)
                    .await
                    .map_err(anyhow::Error::msg)
            }
        }
    }

    /// Probe whether every unfinished background-task record for `session`
    /// was left by a now-dead process. Server attach uses this to choose
    /// between `Consume` (inject the "killed with the process" notice,
    /// exactly like TUI/CLI restart) and `Preserve` (the session may still
    /// be live in another process, which clears its own records): the
    /// notice is only injected when the previous owner is definitely dead.
    /// Conservative: any uncertainty (a live owner, an old record without
    /// an owner, a probe failure) reports false → the caller keeps
    /// `Preserve` and never misreports. No records → true (Consume is a
    /// no-op).
    pub async fn unfinished_owner_all_dead(&self, root: &Path, session: &str) -> Result<bool> {
        match self {
            SessionStore::Jsonl => Ok(Session::unfinished_owner_all_dead(root, session)),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime {
                session: greptime_session,
                ..
            } => {
                let session_id = session.to_owned();
                greptime_session
                    .lock()
                    .await
                    .unfinished_owner_all_dead(&session_id)
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
                    .unfinished_owner_all_dead(&session_id)
                    .await
                    .map_err(anyhow::Error::msg)
            }
        }
    }

    /// Like [`Self::take_unfinished_background`] but scoped to rows whose
    /// `subagent_session_id` matches — used when resuming a subagent
    /// session so it learns what died with its background delegates. The
    /// Greptime/SQLite table is global and supports the cross-session
    /// lookup; JSONL has no per-subagent record file, so it always reports
    /// nothing.
    pub async fn take_unfinished_background_for_subagent(
        &self,
        _root: &Path,
        _subagent_session_id: &str,
    ) -> Result<Vec<String>> {
        match self {
            SessionStore::Jsonl => Ok(Vec::new()),
            #[cfg(feature = "greptime")]
            SessionStore::Greptime { session, .. } => {
                session
                    .lock()
                    .await
                    .take_unfinished_tasks_for_subagent(_subagent_session_id)
                    .await
            }
            #[cfg(feature = "sqlite")]
            SessionStore::Sqlite { session, .. } => session
                .lock()
                .await
                .take_unfinished_tasks_for_subagent(_subagent_session_id)
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
                    .lock()
                    .await
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
    /// matches — record lines are removed on task completion
    /// (`clear_background_task`), mirroring `running_tasks`.
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
                session
                    .lock()
                    .await
                    .label_for_subagent(_subagent_session_id)
                    .await
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
            SessionStore::Greptime { session, .. } => {
                session.lock().await.all_subagent_labels().await.map(Some)
            }
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
/// sidecar does not exist or holds no parseable line. Corrupt lines are
/// skipped: the file is append-only and only the tail matters (a corrupt
/// tail is treated as "no newer snapshot").
fn jsonl_read_meta_snapshot(root: &Path, session: &str) -> anyhow::Result<Option<SessionMeta>> {
    let path = jsonl_meta_sidecar_path(root, session)?;
    let Ok(file) = std::fs::File::open(&path) else {
        return Ok(None);
    };
    let mut latest: Option<SessionMeta> = None;
    for line in std::io::BufReader::new(file)
        .lines()
        .map_while(|line| line.ok())
    {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(meta) = serde_json::from_str::<SessionMeta>(&line) {
            latest = Some(meta);
        }
    }
    Ok(latest)
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
    if let Some(existing) = jsonl_read_meta_snapshot(root, session)? {
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
                writer: None, // stamped by jsonl_append_meta_snapshot with the writing process
                label: None, // label lives in running_tasks / background records, resolved at list time
            };
            return jsonl_append_meta_snapshot(root, session, &mut meta);
        }
        return Ok(()); // resume: nothing missing, never rewrite created_at
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
    let Some(mut meta) = jsonl_read_meta_snapshot(root, session)? else {
        return Ok(()); // R3: never self-create
    };
    mutate(&mut meta);
    meta.last_active_at = chrono::Utc::now().naive_utc();
    jsonl_append_meta_snapshot(root, session, &mut meta)
}

/// `touch_meta` for the JSONL backend: append one fresh snapshot line with
/// `last_active_at = now` and `entry_count` = the current transcript line
/// count, or no-op when no sidecar exists (R3 — a sidecar-less session
/// must not fabricate its own row). The transcript is re-counted on every
/// call: the store is stateless, and the count is the exact JSONL analogue
/// of the DB's `next_seq`. Called synchronously at turn boundaries (like
/// `record_background_start`); the read is a few hundred KB at worst.
fn jsonl_touch_meta(root: &Path, session: &str) -> anyhow::Result<()> {
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
    let directory = root.join(".e-agent/sessions");
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return Ok(Vec::new()); // no sessions directory yet
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Session ids cannot contain `.`, so `<id>.meta.jsonl` is
        // unambiguous: the only files with this suffix are sidecars.
        let Some(session_id) = name.strip_suffix(".meta.jsonl") else {
            continue;
        };
        if let Some(meta) = jsonl_read_meta_snapshot(root, session_id)? {
            out.push(meta);
        }
    }
    out.sort_by(|a, b| b.last_active_at.cmp(&a.last_active_at));
    Ok(out)
}

/// `delete_meta` for the JSONL backend: remove the session's sidecar file.
/// The transcript stays, so resume still works. A missing sidecar is `Ok`
/// (idempotent, matching the DB's zero-row DELETE). Known limitation
/// (mirroring the audit-log design without tombstones): the next
/// `backfill_sessions` bootstrap run re-creates the file from the
/// transcript, so hiding is scoped to the current server lifetime.
fn jsonl_delete_meta(root: &Path, session: &str) -> anyhow::Result<()> {
    let path = jsonl_meta_sidecar_path(root, session)?;
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
/// on their next resume).
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
        if jsonl_read_meta_snapshot(root, &session)?.is_some() {
            continue; // already has a snapshot (idempotent)
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
            title: None,    // pre-sidecar sessions have no user-assigned name
            pinned: None,   // pre-sidecar sessions are unpinned
            archived: None, // pre-sidecar sessions are unarchived
            writer: None,   // stamped by jsonl_append_meta_snapshot with the writing process
            label: None, // label lives in running_tasks / background records, resolved at list time
        };
        jsonl_append_meta_snapshot(root, &session, &mut meta)?;
    }
    Ok(())
}

/// `label_for_subagent` for the JSONL backend: scan every surviving
/// `<id>.background.jsonl` record file for a delegate line whose
/// `session_id` matches the subagent, and return the last (newest)
/// matching label. Record lines are removed when the delegate task
/// completes (`clear_background_task`), so a surviving line means the
/// delegate is still live — exactly the `running_tasks` contract. Files
/// are scanned in sorted name order and lines in append order, so "newest"
/// is deterministic; a subagent with several live delegates reports the
/// most recently recorded one. Non-destructive (called from the sessions
/// list, like the DB lookup).
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
            },
            SessionEntry::ForkedFrom {
                source: "src".into(),
                at: 3,
                event_time: None,
                seq: None,
            },
            SessionEntry::Error {
                text: "harness exploded".into(),
            },
        ]
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

    /// `owner_alive` liveness probe: the current process's own identity is
    /// alive; huge non-existent pids are definitely dead; anything that
    /// cannot be judged (foreign hostname, "unknown" hostname, malformed
    /// identity) is alive (conservative — the caller keeps Preserve
    /// instead of misreporting).
    #[test]
    fn owner_alive_is_conservative_and_spot_checks_liveness() {
        // Our own identity: pid exists, hostname matches → alive
        // (kill -0 succeeds on the same uid).
        let me = process_identity();
        assert!(
            owner_alive(me),
            "the current process must probe as alive: {me}"
        );

        // The dead-pid probe path (`kill -0` ESRCH + no /proc entry) is
        // only reachable when the environment exports a real hostname:
        // with no HOSTNAME/COMPUTERNAME either side is "unknown" and
        // owner_alive is unjudgeable → alive (P2-2). Assert the
        // deterministic conservative outcome in that case, and the real
        // probe (kill -0 + /proc fallback) when a hostname is exported
        // (CI runners and most dev shells).
        if let Some(hostname) = probeable_hostname() {
            // Pids that cannot exist (well-formed `pid@hostname#nonce`
            // shape, but far above any real pid_max): `kill -0` reports
            // ESRCH and /proc has no such directory → definitely dead.
            let dead = format!("2000000000@{hostname}#deadbeef");
            assert!(!owner_alive(&dead), "impossible pid must probe as dead");
            let dead2 = format!("3999999999@{hostname}#cafebabe");
            assert!(
                !owner_alive(&dead2),
                "well-formed but non-existent pid must probe as dead"
            );
        } else {
            assert!(
                owner_alive("2000000000@unknown#deadbeef"),
                "unprobeable hostname keeps even a dead pid conservative"
            );
        }

        // Different hostname (even with our own pid) → cannot judge → alive.
        let foreign = format!("{}@some-other-machine#deadbeef", std::process::id());
        assert!(
            owner_alive(&foreign),
            "foreign hostname must be conservative (alive)"
        );

        // A hostname that fell back to "unknown" (either side) cannot be
        // compared across machines → conservative alive, even for a pid
        // that would otherwise probe dead. Sacrificing the bare-environment
        // same-machine notice eliminates the cross-machine false-dead.
        assert!(
            owner_alive(&format!("{}@unknown#nonce", std::process::id())),
            "record-side unknown hostname must be conservative (alive)"
        );
        assert!(
            owner_alive("2000000000@unknown#deadbeef"),
            "unknown hostname wins over a dead pid (conservative)"
        );

        // Malformed identities → cannot judge → alive.
        assert!(owner_alive(""), "empty identity is conservative");
        assert!(
            owner_alive("not-an-identity"),
            "no @ separator is conservative"
        );
        assert!(
            owner_alive("12345@hostname-without-nonce"),
            "no # separator is conservative"
        );
        assert!(
            owner_alive("abc@myhost#nonce"),
            "unparsable pid is conservative"
        );
    }

    /// The macOS stderr-discrimination logic (the no-/proc EPERM-vs-ESRCH
    /// disambiguation) as a pure classifier. Exercised on Linux under
    /// `#[cfg(test)]`; on macOS it is the live probe path. The guarantee
    /// under test: EPERM (alive but other-uid) must never read as dead,
    /// ESRCH is the ONLY way to get false, and any unclassifiable stderr
    /// falls back to conservative alive.
    #[test]
    fn classify_kill_stderr_disambiguates_eperm_from_esrch() {
        // EPERM — process alive but owned by another user → alive.
        assert!(classify_kill_stderr("kill: 123: Operation not permitted"));
        assert!(classify_kill_stderr("kill: 456: Operation not permitted\n"));
        // ESRCH — the only definite-dead answer.
        assert!(!classify_kill_stderr("kill: 2000000000: No such process"));
        assert!(!classify_kill_stderr("No such process"));
        // Unclassifiable → conservative alive, never dead.
        assert!(classify_kill_stderr(""));
        assert!(classify_kill_stderr("kill: 123: something else"));
        assert!(classify_kill_stderr("zsh: killed"));
        // EPERM is checked before the substring "permitted" could ever
        // collide with an ESRCH message; order is fixed and conservative.
        assert!(classify_kill_stderr(
            "Operation not permitted and No such process"
        ));
    }

    /// The environment's exported hostname, when there is one: only then
    /// can a hand-built identity match `owner_alive`'s hostname check and
    /// reach the pid-probe path (P2-2 makes an unset hostname unjudgeable
    /// → conservative alive).
    fn probeable_hostname() -> Option<String> {
        let host = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .ok()?;
        (!host.is_empty() && host != "unknown").then_some(host)
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

        /// Wait for the fire-and-forget background-record facade to land:
        /// `record_background_start` spawns onto the current runtime and is
        /// never awaited, so poll `take_unfinished_background` (which
        /// consumes) until the row appears or a deadline passes.
        async fn take_unfinished_with_retry(
            store: &SessionStore,
            root: &Path,
            session: &str,
        ) -> Vec<String> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let labels = store
                    .take_unfinished_background(root, session)
                    .await
                    .expect("take unfinished background");
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
                .take_unfinished_background(&root, &session)
                .await
                .expect("final take");
            assert!(labels.is_empty(), "cleared task must not be reported");

            // Subagent-scoped record, looked up from a DIFFERENT session's
            // store (the running_tasks table is workspace-global): the
            // non-consuming label_for_subagent reports it first, then the
            // consuming take_unfinished_background_for_subagent resolves it
            // cross-session.
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

            // The subagent-scoped take keys the lookup by the
            // subagent_session_id, so its labels carry no `(session: …)`
            // suffix.
            let subagent_labels = sub_store
                .take_unfinished_background_for_subagent(&root, "sub-123")
                .await
                .expect("take for subagent");
            assert_eq!(
                subagent_labels,
                vec![crate::session::format_unfinished(9, "delegate work", None)]
            );
            // Consumed: the label lookup is now empty and the by-session
            // take has nothing left for this row either.
            assert_eq!(
                sub_store
                    .label_for_subagent(&root, "sub-123")
                    .await
                    .expect("label after take"),
                None
            );
            let labels = store
                .take_unfinished_background(&root, &session)
                .await
                .expect("by-session take after subagent consume");
            assert!(labels.is_empty());
        }

        /// The probe's Err contract through the facade: when the
        /// `running_tasks` query hard-fails (table dropped behind the
        /// store's back), `unfinished_owner_all_dead` returns Err instead
        /// of panicking — this is exactly the error the server's
        /// `build_session` catches and degrades to Preserve (P1-2), so the
        /// session build never 500s over a broken probe.
        #[tokio::test]
        async fn sqlite_unfinished_owner_all_dead_probe_error_propagates() {
            let (_dir, path) = temp_db();
            let root = std::env::temp_dir();
            let session = format!("test-store-owner-err-{}", crate::session::new_id());
            let store = SessionStore::connect(&backend(&path), &root, &session)
                .await
                .expect("connect sqlite store");

            // Break the running_tasks table from a second raw connection.
            let db = turso::Builder::new_local(path.to_str().unwrap())
                .build()
                .await
                .expect("open db");
            let conn = db.connect().expect("connect raw");
            conn.execute("DROP TABLE running_tasks", ())
                .await
                .expect("drop running_tasks table");
            drop(conn);
            drop(db);

            let err = store
                .unfinished_owner_all_dead(&root, &session)
                .await
                .expect_err("probe must error on a broken table");
            let message = format!("{err:#}");
            assert!(
                message.contains("cannot load unfinished background task owners"),
                "probe error must carry context: {message}"
            );
        }

        /// `inject_killed_notice` 的端到端契约（build_session 的 Consume
        /// 路径；server 启动僵尸扫描已改为两遍汇总、只往父会话注入，不再
        /// 调用它——见 server.rs `scan_zombie_background_tasks`）：dead-owner
        /// 的 running_tasks 行 → 消费 + Notice 追加到存储（无 live agent 也能
        /// 注入）；再次调用幂等返回 None。与 resume 路径同源，天然幂等。
        #[tokio::test]
        async fn sqlite_inject_killed_notice_consumes_and_appends_without_live_agent() {
            let (_dir, path) = temp_db();
            let root = std::env::temp_dir();
            let session = format!("test-store-inject-{}", crate::session::new_id());
            let store = SessionStore::connect(&backend(&path), &root, &session)
                .await
                .expect("connect sqlite store");

            // 只在一个可探测的 hostname 环境下才能构造「确定已死」的 owner
            // （无 HOSTNAME/COMPUTERNAME 时 owner_alive 保守 alive，探测不可
            // 达）；否则断言保守行为：不注入。
            let Some(hostname) = probeable_hostname() else {
                let injected =
                    crate::session_factory::inject_killed_notice(&store, &root, &session)
                        .await
                        .expect("inject with unprobeable hostname");
                assert!(injected.is_none(), "unprobeable hostname must not inject");
                return;
            };
            let dead_owner = format!("2000000000@{hostname}#deadbeef");

            // 直接插两条 owner 全死的 running_tasks 行（record_background_start
            // 写的是当前进程的存活 identity，这里要模拟上次进程的死记录）。
            let workspace_id = derive_workspace_id(&root);
            let db = turso::Builder::new_local(path.to_str().unwrap())
                .build()
                .await
                .expect("open db");
            let conn = db.connect().expect("connect raw");
            for (task_id, label) in [(1u64, "sleep 100"), (2, "cargo build")] {
                conn.execute(
                    "INSERT INTO running_tasks \
                     (workspace_id, session_id, task_id, label, subagent_session_id, \
                      started_at_us, owner_identity) \
                     VALUES (?1, ?2, ?3, ?4, NULL, 1, ?5)",
                    (
                        workspace_id.as_str(),
                        session.as_str(),
                        task_id as i64,
                        label,
                        dead_owner.as_str(),
                    ),
                )
                .await
                .expect("insert dead-owner running_tasks row");
            }
            drop(conn);
            drop(db);

            // 探测：全部 owner 已死。
            assert!(
                store
                    .unfinished_owner_all_dead(&root, &session)
                    .await
                    .expect("probe"),
                "dead-owner rows must probe all-dead"
            );

            // 注入：无 live agent（没有 Agent，只有 store）也能拿到 Notice +
            // 其精确 located key（seq + event_time_us + payload hash）。
            let (entry, location) =
                crate::session_factory::inject_killed_notice(&store, &root, &session)
                    .await
                    .expect("inject killed notice")
                    .expect("rows must inject a notice");
            let SessionEntry::Notice { text } = &entry else {
                panic!("injected entry must be a Notice, got {entry:?}");
            };
            assert!(
                text.contains("2 background task(s)"),
                "count in notice: {text}"
            );
            assert!(text.contains("sleep 100"), "label in notice: {text}");
            assert!(text.contains("cargo build"), "label in notice: {text}");
            // The located key is exact: backend + fingerprint + session +
            // a sqlite seq/event_time pin and a non-empty entry hash.
            assert_eq!(location.backend, "sqlite");
            assert_eq!(location.session, session);
            assert_eq!(location.fingerprint.len(), 32);
            let crate::session_store::LocatedKey::Sqlite { seq, event_time_us } = location.key
            else {
                panic!("sqlite location must carry a seq+event_time_us pin");
            };
            assert!(seq >= 0 && event_time_us > 0);
            assert_eq!(location.entry_hash.len(), 64);

            // 记录已被消费：再 take 为空、再注入为 None（幂等）。
            let labels = store
                .take_unfinished_background(&root, &session)
                .await
                .expect("take after inject");
            assert!(labels.is_empty(), "rows must be consumed: {labels:?}");
            let again = crate::session_factory::inject_killed_notice(&store, &root, &session)
                .await
                .expect("second inject");
            assert!(again.is_none(), "second inject must be a no-op");

            // Notice 已持久化：会话下次加载（resume/attach 的 restore_history）
            // 能看到它——不依赖任何 live agent。
            let loaded = store.load(&root, &session).await.expect("load session");
            let notices: Vec<&SessionEntry> = loaded
                .entries
                .iter()
                .filter(|e| matches!(e, SessionEntry::Notice { .. }))
                .collect();
            assert_eq!(notices.len(), 1, "exactly one persisted Notice");
            assert_eq!(notices[0], &entry, "persisted Notice is the injected one");
        }

        /// Exact-version semantics through the SQLite store: a receipt pins
        /// (seq, event_time_us, payload hash) — a SAME-SEQ later version
        /// (a newer event_time_us row with different payload, appended
        /// directly) must NEVER retarget an old ref. The old receipt still
        /// reads the old physical row; a fresh receipt reads the new one;
        /// deleting the pinned row fails the old ref with integrity.
        #[tokio::test]
        async fn sqlite_same_seq_later_version_never_retargets_old_ref() {
            use crate::agent::Message;
            use crate::output_receipt::{FieldId, ReceiptCodec};

            let (_dir, path) = temp_db();
            let root = std::env::temp_dir();
            let session = format!("test-store-exact-ver-{}", crate::session::new_id());
            let store = SessionStore::connect(&backend(&path), &root, &session)
                .await
                .expect("connect sqlite store");
            let codec = ReceiptCodec::load_from_dir(_dir.path()).unwrap();

            // Version A at seq 0.
            let entry_a = SessionEntry::Message {
                message: Message::User {
                    content: "version-A".into(),
                    images: vec![],
                },
            };
            let locations_a = store
                .append_located(&root, &session, std::slice::from_ref(&entry_a))
                .await
                .expect("append A");
            let loc_a = locations_a[0].clone();
            let LocatedKey::Sqlite {
                seq,
                event_time_us: et_a,
            } = loc_a.key
            else {
                panic!("sqlite location expected");
            };

            // Version B at the SAME seq with a LATER event_time_us and a
            // DIFFERENT payload (simulated concurrent/retry write that
            // landed a newer row for the same logical position).
            let entry_b = SessionEntry::Message {
                message: Message::User {
                    content: "version-B".into(),
                    images: vec![],
                },
            };
            let payload_b = serde_json::to_string(&entry_b).unwrap();
            let db = turso::Builder::new_local(path.to_str().unwrap())
                .build()
                .await
                .expect("open db");
            let conn = db.connect().expect("connect raw");
            conn.execute(
                "INSERT INTO session_entries \
                 (workspace_id, session_id, seq, event_time_us, entry_kind, payload, schema_version, is_error) \
                 VALUES (?1, ?2, ?3, ?4, 'message', ?5, 1, 0)",
                (
                    derive_workspace_id(&root).as_str(),
                    session.as_str(),
                    seq,
                    et_a + 1, // strictly later
                    payload_b.as_str(),
                ),
            )
            .await
            .expect("insert later version B");
            drop(conn);
            drop(db);

            // The READ path shows the newer version (latest event_time wins).
            let loaded = store.load(&root, &session).await.expect("load");
            assert_eq!(loaded.entries.len(), 1);
            assert!(matches!(
                &loaded.entries[0],
                SessionEntry::Message { message: Message::User { content, .. } } if content == "version-B"
            ));
            // load_located reports the WINNING location (seq, later et).
            let located = store
                .load_located(&root, &session)
                .await
                .expect("load_located");
            assert_eq!(located.entries.len(), 1);
            let new_loc = located.locations[0].clone().expect("location");
            assert!(matches!(
                new_loc.key,
                LocatedKey::Sqlite { seq: s, event_time_us: et } if s == seq && et == et_a + 1
            ));
            assert_ne!(new_loc.entry_hash, loc_a.entry_hash);

            // The OLD receipt still reads the OLD physical row exactly.
            let old_ref = codec
                .issue(&loc_a, FieldId::UserContent, "version-A".len())
                .unwrap();
            let verified = codec.verify(&old_ref).unwrap();
            let bytes = store
                .read_field(&root, &verified)
                .await
                .expect("old ref must stay pinned to version A");
            assert_eq!(bytes, b"version-A");

            // A receipt issued for the NEW location reads version B.
            let new_ref = codec
                .issue(&new_loc, FieldId::UserContent, "version-B".len())
                .unwrap();
            let verified = codec.verify(&new_ref).unwrap();
            let bytes = store
                .read_field(&root, &verified)
                .await
                .expect("new ref reads version B");
            assert_eq!(bytes, b"version-B");

            // Deleting the pinned row makes the OLD ref fail with integrity
            // (never a silently retargeted read).
            let db = turso::Builder::new_local(path.to_str().unwrap())
                .build()
                .await
                .expect("open db");
            let conn = db.connect().expect("connect raw");
            conn.execute(
                "DELETE FROM session_entries \
                 WHERE workspace_id = ?1 AND session_id = ?2 AND seq = ?3 AND event_time_us = ?4",
                (
                    derive_workspace_id(&root).as_str(),
                    session.as_str(),
                    seq,
                    et_a,
                ),
            )
            .await
            .expect("delete pinned row");
            drop(conn);
            drop(db);
            let err = store
                .read_field(&root, &codec.verify(&old_ref).unwrap())
                .await
                .expect_err("deleted pinned row must fail the old ref");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("integrity") || msg.contains("not found"),
                "old ref must fail closed: {msg}"
            );
        }

        /// End-to-end read_output reconstruction on the SQLite store: bound
        /// an oversized tool result with a receipt, read the exact full
        /// field through the facade, and page it back byte-exactly.
        #[tokio::test]
        async fn sqlite_receipt_page_reconstruction() {
            use crate::agent::Message;
            use crate::output_receipt::{FieldId, ReceiptCodec, page_field};

            let (_dir, path) = temp_db();
            let root = std::env::temp_dir();
            let session = format!("test-store-page-{}", crate::session::new_id());
            let store = SessionStore::connect(&backend(&path), &root, &session)
                .await
                .expect("connect sqlite store");
            let codec = ReceiptCodec::load_from_dir(_dir.path()).unwrap();
            let content = format!("{}{}{}", "x".repeat(20_000), "MIDDLE", "y".repeat(10_000));
            let entry = SessionEntry::Message {
                message: Message::Tool {
                    call_id: "call_1".into(),
                    name: "bash".into(),
                    content: content.clone(),
                    is_error: false,
                    synthetic: false,
                    images: vec![],
                },
            };
            let locations = store
                .append_located(&root, &session, std::slice::from_ref(&entry))
                .await
                .expect("append");
            let receipt = codec
                .issue(&locations[0], FieldId::ToolContent, content.len())
                .unwrap();
            let verified = codec.verify(&receipt).unwrap();
            let bytes = store
                .read_field(&root, &verified)
                .await
                .expect("read_field");
            assert_eq!(bytes, content.as_bytes());
            // Page chain (offset 0, limit 7000, next) is lossless.
            let mut rebuilt = Vec::new();
            let mut offset = Some(0usize);
            while let Some(next) = offset {
                let page = page_field(&bytes, next, 7000).unwrap();
                assert!(std::str::from_utf8(page.text.as_bytes()).is_ok());
                rebuilt.extend_from_slice(page.text.as_bytes());
                offset = page.next_offset;
            }
            assert_eq!(rebuilt, content.as_bytes());
        }

        /// `load_head_page` on the SQLite store: the bounded head page
        /// must never strand the truncated part of the head segment —
        /// the returned cursor feeds straight back into `load_older` and
        /// the whole chain covers every entry exactly once (the
        /// `GET /history` initial-render bug this fixes).
        #[tokio::test]
        async fn sqlite_load_head_page_pages_without_losing_segments() {
            use crate::agent::Message;

            let (_dir, path) = temp_db();
            let root = std::env::temp_dir();

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

            // ---- No compaction: the whole session is one head segment.
            // With no limit the whole session comes back with a None
            // cursor (nothing older to page)...
            let session_a = format!("test-store-head-page-a-{}", crate::session::new_id());
            let store_a = SessionStore::connect(&backend(&path), &root, &session_a)
                .await
                .expect("connect sqlite store A");
            let plain = vec![user(1), user(2), user(3)];
            store_a
                .append(&root, &session_a, &plain)
                .await
                .expect("append A");
            let (page, cursor) = store_a
                .load_head_page(&root, &session_a, None)
                .await
                .expect("load_head_page A without limit");
            assert_eq!(page, plain, "no-compaction session returned whole");
            assert_eq!(cursor, None, "no compaction → no older cursor");
            // ...and a bounded page still pages through the rest of the
            // session (nothing is stranded).
            let (page, cursor) = store_a
                .load_head_page(&root, &session_a, Some(2))
                .await
                .expect("load_head_page A with limit");
            assert_eq!(page, vec![user(2), user(3)], "newest limit entries");
            assert_eq!(cursor, Some(1), "cursor = oldest seq of the page");
            let (rest, cursor) = store_a
                .load_older(&root, &session_a, cursor.unwrap(), Some(2))
                .await
                .expect("load_older A");
            assert_eq!(rest, vec![user(1)], "remaining entry reachable");
            assert_eq!(cursor, None, "seq 0 page → nothing older");

            // ---- Compaction with head ≤ limit: whole head segment +
            // cursor = the opening compaction's seq.
            let session_b = format!("test-store-head-page-b-{}", crate::session::new_id());
            let store_b = SessionStore::connect(&backend(&path), &root, &session_b)
                .await
                .expect("connect sqlite store B");
            // seqs: 0,1 early; 2 comp1; 3,4 middle; 5 comp2; 6,7 latest.
            let mut all = vec![user(1), user(2)];
            all.push(comp("c1"));
            all.extend([user(3), user(4)]);
            all.push(comp("c2"));
            all.extend([user(5), user(6)]);
            store_b
                .append(&root, &session_b, &all)
                .await
                .expect("append B");
            let (page, cursor) = store_b
                .load_head_page(&root, &session_b, Some(3))
                .await
                .expect("load_head_page B with limit");
            assert_eq!(
                page,
                vec![comp("c2"), user(5), user(6)],
                "head ≤ limit → whole head segment"
            );
            assert_eq!(cursor, Some(5), "cursor = opening compaction seq");

            // ---- Compaction with head > limit: newest `limit` entries,
            // cursor = oldest seq of the page; paging back with that
            // cursor reaches the cut-off part, then crosses compaction
            // boundaries — every entry is covered exactly once.
            let session_c = format!("test-store-head-page-c-{}", crate::session::new_id());
            let store_c = SessionStore::connect(&backend(&path), &root, &session_c)
                .await
                .expect("connect sqlite store C");
            // seqs: 0,1 early; 2 comp1; 3,4 middle; 5 comp2; 6..9 latest.
            let mut all = vec![user(1), user(2)];
            all.push(comp("c1"));
            all.extend([user(3), user(4)]);
            all.push(comp("c2"));
            all.extend([user(5), user(6), user(7), user(8)]);
            store_c
                .append(&root, &session_c, &all)
                .await
                .expect("append C");

            let mut paged: Vec<SessionEntry> = Vec::new();
            let mut cursor: Option<i64> = Some(i64::MAX); // head open sentinel
            loop {
                let (entries, next) = match cursor {
                    Some(i64::MAX) => store_c
                        .load_head_page(&root, &session_c, Some(2))
                        .await
                        .expect("head page"),
                    Some(before) => store_c
                        .load_older(&root, &session_c, before, Some(2))
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

            // Spot-check the boundary page explicitly: the first page is
            // the newest 2 of the head, and its cursor pages into the
            // cut-off head part (not past the head into older segments).
            let (page, cursor) = store_c
                .load_head_page(&root, &session_c, Some(2))
                .await
                .expect("head page C");
            assert_eq!(page, vec![user(7), user(8)], "newest 2 of head");
            let (next, _) = store_c
                .load_older(&root, &session_c, cursor.unwrap(), Some(2))
                .await
                .expect("older page C");
            assert_eq!(
                next,
                vec![user(5), user(6)],
                "cut-off head part reachable via cursor"
            );
        }

        /// End-to-end: a real tool-call/tool-result session written through
        /// the store, fully closed, then reopened (simulating a TUI/process
        /// restart) and replayed — every entry back, in order, with the
        /// compaction-aware segmented views intact.
        #[tokio::test]
        async fn sqlite_e2e_reopen_replays_full_tool_session() {
            use crate::agent::{AssistantMessage, Message};

            let (_dir, path) = temp_db();
            let root = std::env::temp_dir();
            let session = format!("test-e2e-replay-{}", crate::session::new_id());

            let tool_call = Message::Assistant(AssistantMessage {
                content: Some("let me check".into()),
                tool_calls: vec![crate::agent::ToolCall {
                    id: "call_1".into(),
                    name: "bash".into(),
                    arguments: "ls".into(),
                }],
                reasoning: Some("thinking".into()),
            })
            .into();
            let tool_result = Message::Tool {
                call_id: "call_1".into(),
                name: "bash".into(),
                content: "src\n".into(),
                is_error: false,
                synthetic: false,
                images: vec![],
            }
            .into();
            let tool_error = Message::Tool {
                call_id: "call_2".into(),
                name: "bash".into(),
                content: "command not found: nope".into(),
                is_error: true,
                synthetic: false,
                images: vec![],
            }
            .into();
            let entries = vec![
                Message::System {
                    content: "You are an agent".into(),
                }
                .into(),
                Message::User {
                    content: "run ls".into(),
                    images: vec![],
                }
                .into(),
                tool_call,
                tool_result,
                tool_error,
                SessionEntry::Notice {
                    text: "background task 3 completed".into(),
                },
                // Compaction in the MIDDLE: the head segment = compaction +
                // everything after it, the older segment = everything before.
                SessionEntry::Compaction {
                    summary: "compressed".into(),
                    retained: vec![],
                    current_prompt_at: None,
                    no_current_prompt: false,
                },
                Message::User {
                    content: "and now?".into(),
                    images: vec![],
                }
                .into(),
                Message::Assistant(AssistantMessage {
                    content: Some("all good".into()),
                    tool_calls: vec![],
                    reasoning: None,
                })
                .into(),
            ];

            // Writer A: connect, append, fully close (drop).
            {
                let store = SessionStore::connect(&backend(&path), &root, &session)
                    .await
                    .expect("connect e2e store A");
                store
                    .append(&root, &session, &entries)
                    .await
                    .expect("append e2e session");
            }

            // Writer B: full reopen on the same file — the TUI/process
            // restart view. Everything must come back in order.
            let store = SessionStore::connect(&backend(&path), &root, &session)
                .await
                .expect("reconnect e2e store B");
            let loaded = store.load(&root, &session).await.expect("replay load");
            assert_eq!(loaded.entries.len(), entries.len(), "no entries lost");
            for (i, (got, want)) in loaded.entries.iter().zip(entries.iter()).enumerate() {
                assert_eq!(got, want, "entry {i} must replay identically");
            }

            // Seq provenance is contiguous 0..n after the restart.
            let with_seq = store
                .load_with_seq(&root, &session)
                .await
                .expect("load_with_seq after restart");
            assert_eq!(with_seq.len(), entries.len());
            for (i, (seq, _)) in with_seq.iter().enumerate() {
                assert_eq!(*seq, i as i64, "contiguous seq after restart");
            }

            // Compaction-aware segmentation survives the restart: head =
            // [compaction .. end], older = [start .. compaction).
            let head = store
                .load_head(&root, &session)
                .await
                .expect("load_head after restart");
            assert_eq!(head.entries, entries[6..], "head = compaction + tail");
            let (older, cursor) = store
                .load_older(&root, &session, 6, None)
                .await
                .expect("load_older after restart");
            assert_eq!(older, entries[..6], "older = everything before compaction");
            assert_eq!(cursor, None, "oldest segment reached");
            assert_eq!(
                store.head_seq(&root, &session).await.expect("head_seq"),
                Some(6),
                "head opens at the compaction seq"
            );
        }

        /// End-to-end concurrent writers through the store: two live stores
        /// on the same file+session both believe they own seq 0; the second
        /// writer to land wins, the stale one is rejected with the
        /// `concurrent write conflict` contract substring and writes
        /// nothing.
        #[tokio::test]
        async fn sqlite_e2e_concurrent_writers_through_store_one_rejected() {
            use crate::agent::Message;

            let (_dir, path) = temp_db();
            let root = std::env::temp_dir();
            let session = format!("test-e2e-conflict-{}", crate::session::new_id());

            let writer_a = |prefix: &str| -> Vec<SessionEntry> {
                vec![
                    Message::User {
                        content: format!("{prefix}-1"),
                        images: vec![],
                    }
                    .into(),
                    Message::User {
                        content: format!("{prefix}-2"),
                        images: vec![],
                    }
                    .into(),
                ]
            };

            // Both connect before anything is written: each derives
            // next_seq = 0 from the (empty) DB file.
            let store_a = SessionStore::connect(&backend(&path), &root, &session)
                .await
                .expect("connect writer A");
            let store_b = SessionStore::connect(&backend(&path), &root, &session)
                .await
                .expect("connect writer B");

            // B lands first: seqs 0,1 with B's payload.
            store_b
                .append(&root, &session, &writer_a("b"))
                .await
                .expect("writer B append lands");

            // A is stale (still thinks it owns seq 0) and appends DIFFERENT
            // content for the same seqs → the concurrent-write detection
            // must reject it before any INSERT.
            let err = store_a
                .append(&root, &session, &writer_a("a"))
                .await
                .expect_err("stale writer must be rejected");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("concurrent write conflict"),
                "contract substring missing: {msg}"
            );
            assert!(msg.contains(&session), "error must name the session: {msg}");

            // The rejected writer wrote nothing: a fresh reopen sees only
            // B's rows, in order.
            let store_c = SessionStore::connect(&backend(&path), &root, &session)
                .await
                .expect("connect writer C");
            let loaded = store_c
                .load(&root, &session)
                .await
                .expect("load after rejection");
            assert_eq!(loaded.entries, writer_a("b"), "only B's rows survive");
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
                .take_unfinished_background(&root, &session)
                .await
                .expect("take after clear+restart");
            assert!(
                labels.is_empty(),
                "cleared task must not reappear after restart: {labels:?}"
            );
        }
    }
}

#[cfg(test)]
#[path = "session_jsonl_meta_tests.rs"]
mod jsonl_meta_tests;
