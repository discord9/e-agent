//! Manual incremental import tool: JSONL → GreptimeDB.
//!
//! Usage:
//!   e-agent-import-jsonl --session ID [--workspace PATH] [--conn CONN] \
//!       [--dry-run]
//!
//! Connects to GreptimeDB, compares the existing DB entries against the
//! local JSONL file, and **only** appends the missing tail when the prefix
//! matches exactly. Safe for repeated (idempotent) runs against a session
//! that was partially imported.
//!
//! ## Safety
//!
//! - The tool refuses to write if the existing DB prefix diverges from
//!   the JSONL file, or if DB has more entries than JSONL.
//! - The target session must be **idle** during import: the tool re-reads
//!   the DB immediately before the write to detect concurrent changes
//!   (TOCTOU mitigation), but GreptimeDB has no transactions, so concurrent
//!   writers could race between the re-read and the INSERT.
//! - Partial commit across chunks (>9000 entries) is possible but
//!   acceptable: just fix the issue and re-run the tool.
//! - dry-run connects to GreptimeDB (which may CREATE TABLE) but never
//!   inserts any rows.

use std::path::PathBuf;

use anyhow::{Context, anyhow, bail};

use e_agent::agent::SessionEntry;
use e_agent::config::{Config, SessionBackend};
use e_agent::session::{Session, validate_session_name};
use e_agent::session_greptime::{GreptimeSession, derive_workspace_id};

// ---------------------------------------------------------------------------
// Pure planning logic (testable without a live DB)
// ---------------------------------------------------------------------------

/// Result of comparing DB entries vs JSONL entries.
#[derive(Debug, PartialEq)]
enum ImportPlan<'a> {
    /// DB matches the JSONL prefix; the remaining entries should be appended.
    Append {
        missing: &'a [SessionEntry],
        db_len: usize,
    },
    /// DB has more entries than JSONL — refuse to truncate.
    DbLonger { db_len: usize, jsonl_len: usize },
    /// DB and JSONL diverge at a specific index.
    Diverged { at: usize },
    /// DB and JSONL are identical in content and length.
    Unchanged,
}

/// Compare deduplicated DB entries with the full JSONL entry list, and
/// decide what to do.
///
/// **Pre-condition**: `db_entries` should already be rebuilt in seq identity
/// order 0..N by `entries_in_seq_order`.
fn plan_import<'a>(
    db_entries: &[SessionEntry],
    jsonl_entries: &'a [SessionEntry],
) -> ImportPlan<'a> {
    if db_entries.len() > jsonl_entries.len() {
        return ImportPlan::DbLonger {
            db_len: db_entries.len(),
            jsonl_len: jsonl_entries.len(),
        };
    }
    for i in 0..db_entries.len() {
        if db_entries[i] != jsonl_entries[i] {
            return ImportPlan::Diverged { at: i };
        }
    }
    let missing = &jsonl_entries[db_entries.len()..];
    if missing.is_empty() {
        ImportPlan::Unchanged
    } else {
        ImportPlan::Append {
            missing,
            db_len: db_entries.len(),
        }
    }
}

/// Validate that seq identities form the continuous set 0..N, then rebuild
/// payloads in seq order for JSONL ordinal comparison.
fn entries_in_seq_order(entries: &[(i64, SessionEntry)]) -> anyhow::Result<Vec<SessionEntry>> {
    let mut entries = entries.to_vec();
    entries.sort_unstable_by_key(|(seq, _)| *seq);
    for (i, (seq, _)) in entries.iter().enumerate() {
        let expected = i as i64;
        if *seq != expected {
            anyhow::bail!(
                "seq discontinuity: expected seq {expected}, got {seq}; \
                 DB has gaps or duplicates that dedup cannot resolve"
            );
        }
    }
    Ok(entries.into_iter().map(|(_, entry)| entry).collect())
}

// ---------------------------------------------------------------------------
// CLI argument parsing (no clap dependency)
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
struct Args {
    workspace: PathBuf,
    session: String,
    conn: Option<String>,
    dry_run: bool,
}

/// Parse arguments from an iterator of raw strings (typically
/// `std::env::args().skip(1)` collected into a `Vec<String>`).
///
/// Both the production `parse_args()` and unit tests call this single
/// implementation.
fn parse_args_from(raw: &[String]) -> anyhow::Result<Args> {
    if raw.is_empty() || raw.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "usage: e-agent-import-jsonl --session ID [--workspace PATH] [--conn CONN] \
             [--dry-run]\n\
             \n\
             Import a JSONL session file into GreptimeDB incrementally.\n\
             Only appends entries that do not yet exist; refuses any\n\
             divergence or truncation.\n\
             \n\
             --session ID              Session name (required, must match [a-zA-Z0-9_-]+)\n\
             --workspace PATH          Workspace root (default: current directory)\n\
             --conn CONN               GreptimeDB pg-wire connection string\n\
                                       (default: from config [session] backend)\n\
             --dry-run                 Print what would be done without inserting rows.\n\
                                       NOTE: still connects to DB which may CREATE TABLE.\n\
             --help                    Show this help\n\
             \n\
             The target session must be IDLE during import. Partial commits\n\
             across large chunks (>9000 entries) are possible; fix the issue\n\
             and re-run."
        );
        return Err(anyhow!("__help__"));
    }

    let mut workspace: Option<PathBuf> = None;
    let mut session: Option<String> = None;
    let mut conn: Option<String> = None;
    let mut dry_run = false;

    let mut iter = raw.iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--workspace" => {
                let val = iter
                    .next()
                    .ok_or_else(|| anyhow!("--workspace requires a value"))?;
                workspace = Some(PathBuf::from(val));
            }
            "--session" => {
                let val = iter
                    .next()
                    .ok_or_else(|| anyhow!("--session requires a value"))?;
                session = Some(val.clone());
            }
            "--conn" => {
                let val = iter
                    .next()
                    .ok_or_else(|| anyhow!("--conn requires a value"))?;
                conn = Some(val.clone());
            }
            "--dry-run" => {
                dry_run = true;
            }
            "--help" | "-h" => {
                // Already handled above.
            }
            "--replace-divergent-tail" => {
                bail!(
                    "--replace-divergent-tail has been removed; \
                     cross-process wall-clock timestamps cannot safely shadow \
                     old entries. If your DB diverges from JSONL, start a new \
                     session or diagnose manually."
                );
            }
            other => {
                bail!("unexpected argument: {other}");
            }
        }
    }

    let session = session.ok_or_else(|| anyhow!("--session is required"))?;
    let workspace =
        workspace.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    Ok(Args {
        workspace,
        session,
        conn,
        dry_run,
    })
}

/// Production entry — delegates to `parse_args_from` with the real argv.
fn parse_args() -> anyhow::Result<Args> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    parse_args_from(&raw)
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let _ = e_agent::init_logging();
    if let Err(err) = run() {
        // --help prints its own text then signals via __help__.
        let msg = format!("{err:#}");
        if msg == "__help__" {
            std::process::exit(0);
        }
        eprintln!("e-agent-import-jsonl: error: {msg}");
        std::process::exit(1);
    }
}

#[tokio::main]
async fn run() -> anyhow::Result<()> {
    let args = parse_args()?;

    // --- Canonicalise workspace ---
    let workspace = args
        .workspace
        .canonicalize()
        .context("cannot canonicalize --workspace")?;
    let workspace_id = derive_workspace_id(&workspace);

    // --- Validate session name ---
    validate_session_name(&args.session).context("invalid --session name")?;

    // --- Require JSONL file to exist ---
    let jsonl_path = workspace
        .join(".e-agent/sessions")
        .join(format!("{}.jsonl", args.session));
    if !jsonl_path.exists() {
        bail!(
            "JSONL session file not found: {}; \
             only .e-agent/sessions/<id>.jsonl is supported (no legacy .json)",
            jsonl_path.display()
        );
    }

    // --- Load JSONL ---
    let loaded = Session::load(&workspace, &args.session)
        .with_context(|| format!("cannot load session '{}' from JSONL", args.session))?;

    if loaded.legacy {
        bail!(
            "session '{sid}' is in legacy JSON format; \
             launch e-agent once to migrate it before importing",
            sid = args.session
        );
    }
    let jsonl_entries = loaded.entries;

    // --- Resolve connection string ---
    let conn = match args.conn.as_deref() {
        Some(c) => c.to_owned(),
        None => {
            let config = Config::load()
                .context("cannot load config; provide --conn or ensure a config file exists")?
                .ok_or_else(|| {
                    anyhow!(
                        "no config file found and --conn not provided; \
                         create ~/.config/e-agent/config.toml with a [session] section or pass --conn"
                    )
                })?;
            match config.session_backend() {
                SessionBackend::Greptime { conn } => conn,
                other => {
                    bail!(
                        "config [session] backend is '{other:?}', not 'greptime'; \
                         provide --conn to override"
                    );
                }
            }
        }
    };

    // --- Connect to GreptimeDB ---
    let mut db = GreptimeSession::connect(&conn, &workspace_id, &args.session)
        .await
        .context("cannot connect to GreptimeDB")?;

    // --- Load existing DB entries with seq numbers ---
    let db_with_seq = db
        .load_with_seq()
        .await
        .context("cannot load session entries from GreptimeDB")?;

    // --- Validate seq identities and rebuild JSONL ordinal order ---
    let db_entry_slice =
        entries_in_seq_order(&db_with_seq).context("DB seq continuity check failed")?;

    // --- Plan ---
    let plan = plan_import(&db_entry_slice, &jsonl_entries);

    // --- Log what we see (no conn credential leak) ---
    println!("session:  {}", args.session);
    println!("workspace: {}", workspace.display());
    println!("DB before: {} entries", db_entry_slice.len());
    println!("JSONL:     {} entries", jsonl_entries.len());

    match plan {
        ImportPlan::DbLonger { db_len, jsonl_len } => {
            bail!(
                "DB has {db_len} entries but JSONL has only {jsonl_len}; \
                 refusing to truncate",
            );
        }
        ImportPlan::Diverged { at } => {
            bail!(
                "DB and JSONL diverge at entry {at}; refusing to import \
                 mismatched prefix.\n\
                 This tool can only append (no repair). Start a new session \
                 or diagnose the divergence manually.\n\
                 If you have a divergent last entry, note that \
                 --replace-divergent-tail has been removed as unsafe.",
            );
        }
        ImportPlan::Unchanged => {
            println!(
                "unchanged — all {n} entries already present in DB",
                n = db_entry_slice.len()
            );
        }
        ImportPlan::Append { missing, db_len } => {
            let range_start = db_len;
            let range_end = db_len + missing.len();
            println!(
                "append: {} entries (positions {range_start}..{range_end})",
                missing.len(),
            );

            if args.dry_run {
                println!(
                    "dry-run: would append {} entries, not writing. \
                     (Connection established — CREATE TABLE may have been executed.)",
                    missing.len()
                );
            } else {
                // TOCTOU guard: re-read DB before writing, validate seq identities,
                // and compare the snapshot in JSONL ordinal order. Skip this in
                // dry-run mode (no insert planned).
                let check = db
                    .load_with_seq()
                    .await
                    .context("cannot re-load DB entries before write (TOCTOU check)")?;
                let check_entries = entries_in_seq_order(&check)
                    .context("DB seq continuity check failed before write (TOCTOU)")?;
                if check_entries != db_entry_slice {
                    bail!(
                        "DB changed between initial read and pre-write check \
                         ({} entries vs {}). Target session may not be idle. \
                         Re-run when session is idle.",
                        db_entry_slice.len(),
                        check_entries.len(),
                    );
                }

                // Sync next_seq from the TOCTOU-re-read snapshot so that
                // stale connect-time next_seq does not cause overwrites.
                db.advance_next_seq_from_snapshot_len(check_entries.len())
                    .context("cannot advance next_seq from TOCTOU snapshot")?;

                db.append(missing)
                    .await
                    .context("cannot append entries to GreptimeDB")?;

                verify_db_matches(&db, &jsonl_entries).await?;

                println!(
                    "appended {} entries — DB now matches JSONL ({n} entries)",
                    missing.len(),
                    n = jsonl_entries.len(),
                );
            }
        }
    }

    Ok(())
}

/// Reload the DB and verify it exactly matches `expected`.
///
/// Uses `load_with_seq` to also validate that stored seq values are
/// strictly 0..N continuous. Error messages distinguish seq continuity
/// failures from content mismatches; all carry a partial-commit warning.
async fn verify_db_matches(db: &GreptimeSession, expected: &[SessionEntry]) -> anyhow::Result<()> {
    let final_entries = db
        .load_with_seq()
        .await
        .context("cannot reload session entries after write for verification")?;

    // 1. Verify seq continuity and rebuild JSONL ordinal order.
    let got_entries = match entries_in_seq_order(&final_entries) {
        Ok(entries) => entries,
        Err(e) => {
            bail!(
                "verification FAILED — {e}; \
                 partial commit may have occurred. Fix the issue and re-run.",
            );
        }
    };

    // 2. Verify length
    if got_entries.len() != expected.len() {
        bail!(
            "verification FAILED: reloaded DB has {} entries, expected {}; \
             partial commit may have occurred. Fix the issue, ensure session \
             is idle, and re-run.",
            got_entries.len(),
            expected.len(),
        );
    }

    // 3. Verify content per seq identity
    for (i, (got, want)) in got_entries.iter().zip(expected.iter()).enumerate() {
        if got != want {
            bail!(
                "verification FAILED at position {i}: DB entry content differs from JSONL; \
                 partial commit may have occurred. Fix the issue and re-run.",
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use e_agent::agent::{AssistantMessage, Message};

    fn msg(content: &str) -> SessionEntry {
        Message::User {
            content: content.to_owned(),
            images: vec![],
        }
        .into()
    }

    fn assistant(content: &str) -> SessionEntry {
        Message::Assistant(AssistantMessage {
            content: Some(content.to_owned()),
            tool_calls: vec![],
            reasoning: None,
        })
        .into()
    }

    fn entries(strings: &[&str]) -> Vec<SessionEntry> {
        strings.iter().map(|s| msg(s)).collect()
    }

    // ------------------------------------------------------------------
    // plan_import (payload-stream comparison after seq-0..N dedup)
    // ------------------------------------------------------------------

    #[test]
    fn plan_unchanged() {
        let db = entries(&["a", "b", "c"]);
        let jsonl = entries(&["a", "b", "c"]);
        assert_eq!(plan_import(&db, &jsonl), ImportPlan::Unchanged);
    }

    #[test]
    fn plan_empty_both() {
        let db: Vec<SessionEntry> = vec![];
        let jsonl: Vec<SessionEntry> = vec![];
        assert_eq!(plan_import(&db, &jsonl), ImportPlan::Unchanged);
    }

    #[test]
    fn plan_append_tail() {
        let db = entries(&["a", "b"]);
        let jsonl = entries(&["a", "b", "c", "d"]);
        match plan_import(&db, &jsonl) {
            ImportPlan::Append { missing, db_len } => {
                assert_eq!(db_len, 2);
                assert_eq!(missing, &entries(&["c", "d"]));
            }
            other => panic!("expected Append, got {other:?}"),
        }
    }

    #[test]
    fn plan_append_from_empty_db() {
        let db: Vec<SessionEntry> = vec![];
        let jsonl = entries(&["a", "b"]);
        match plan_import(&db, &jsonl) {
            ImportPlan::Append { missing, db_len } => {
                assert_eq!(db_len, 0);
                assert_eq!(missing, &entries(&["a", "b"]));
            }
            other => panic!("expected Append, got {other:?}"),
        }
    }

    #[test]
    fn plan_db_longer() {
        let db = entries(&["a", "b", "c"]);
        let jsonl = entries(&["a", "b"]);
        match plan_import(&db, &jsonl) {
            ImportPlan::DbLonger { db_len, jsonl_len } => {
                assert_eq!(db_len, 3);
                assert_eq!(jsonl_len, 2);
            }
            other => panic!("expected DbLonger, got {other:?}"),
        }
    }

    #[test]
    fn plan_diverged_at_start() {
        let db = entries(&["x", "b"]);
        let jsonl = entries(&["a", "b"]);
        match plan_import(&db, &jsonl) {
            ImportPlan::Diverged { at } => assert_eq!(at, 0),
            other => panic!("expected Diverged, got {other:?}"),
        }
    }

    #[test]
    fn plan_diverged_in_middle() {
        let db = entries(&["a", "x", "c"]);
        let jsonl = entries(&["a", "b", "c"]);
        match plan_import(&db, &jsonl) {
            ImportPlan::Diverged { at } => assert_eq!(at, 1),
            other => panic!("expected Diverged, got {other:?}"),
        }
    }

    #[test]
    fn plan_diverged_at_last() {
        let db = entries(&["a", "b", "x"]);
        let jsonl = entries(&["a", "b", "c"]);
        match plan_import(&db, &jsonl) {
            ImportPlan::Diverged { at } => assert_eq!(at, 2),
            other => panic!("expected Diverged, got {other:?}"),
        }
    }

    #[test]
    fn plan_diverged_variants_still_compared() {
        // Different entry types at the same position must also diverge.
        let db = vec![assistant("hello")];
        let jsonl = entries(&["hello"]);
        match plan_import(&db, &jsonl) {
            ImportPlan::Diverged { at } => assert_eq!(at, 0),
            other => panic!("expected Diverged, got {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // entries_in_seq_order
    // ------------------------------------------------------------------

    #[test]
    fn seq_continuity_ok() {
        let data: Vec<(i64, SessionEntry)> = (0..5).map(|i| (i, msg("x"))).collect();
        assert!(entries_in_seq_order(&data).is_ok());
    }

    #[test]
    fn canonical_event_time_order_rebuilds_jsonl_seq_order() {
        let data = vec![(2, msg("c")), (0, msg("a")), (1, msg("b"))];
        let rebuilt = entries_in_seq_order(&data).unwrap();
        let jsonl = entries(&["a", "b", "c"]);
        assert_eq!(rebuilt, jsonl);
        assert_eq!(plan_import(&rebuilt, &jsonl), ImportPlan::Unchanged);
    }

    #[test]
    fn seq_continuity_empty() {
        let data: Vec<(i64, SessionEntry)> = vec![];
        assert!(entries_in_seq_order(&data).is_ok());
    }

    #[test]
    fn seq_continuity_gap() {
        let data: Vec<(i64, SessionEntry)> = vec![
            (0, msg("a")),
            (2, msg("b")), // gap: seq 1 missing
        ];
        let err = entries_in_seq_order(&data).unwrap_err();
        assert!(format!("{err:#}").contains("seq discontinuity: expected seq 1"));
    }

    #[test]
    fn seq_continuity_wrong_start() {
        let data: Vec<(i64, SessionEntry)> = vec![
            (1, msg("a")), // should start at 0
        ];
        let err = entries_in_seq_order(&data).unwrap_err();
        assert!(format!("{err:#}").contains("seq discontinuity: expected seq 0"));
    }

    #[test]
    fn seq_continuity_duplicate_seq() {
        let data: Vec<(i64, SessionEntry)> = vec![
            (0, msg("a")),
            (0, msg("b")), // duplicate
        ];
        // Dedup handled in load_with_seq, but verify catches if duplicates
        // somehow survive.
        let err = entries_in_seq_order(&data).unwrap_err();
        assert!(format!("{err:#}").contains("seq discontinuity: expected seq 1"));
    }

    // ------------------------------------------------------------------
    // parse_args_from (single implementation for CLI and tests)
    // ------------------------------------------------------------------

    #[test]
    fn parse_basic_workspace_and_session() {
        let raw = vec![
            "--workspace".into(),
            "/my/ws".into(),
            "--session".into(),
            "my-session_01".into(),
        ];
        let args = parse_args_from(&raw).unwrap();
        assert_eq!(args.workspace, PathBuf::from("/my/ws"));
        assert_eq!(args.session, "my-session_01");
        assert!(args.conn.is_none());
        assert!(!args.dry_run);
    }

    #[test]
    fn parse_dry_run() {
        let raw = vec!["--session".into(), "s1".into(), "--dry-run".into()];
        let args = parse_args_from(&raw).unwrap();
        assert!(args.dry_run);
        assert_eq!(args.session, "s1");
    }

    #[test]
    fn parse_conn_provided() {
        let raw = vec![
            "--session".into(),
            "s1".into(),
            "--conn".into(),
            "host=10.0.0.1 port=4002".into(),
        ];
        let args = parse_args_from(&raw).unwrap();
        assert_eq!(args.conn.as_deref(), Some("host=10.0.0.1 port=4002"));
    }

    #[test]
    fn parse_session_missing() {
        let raw: Vec<String> = vec!["--workspace".into(), "/tmp".into()];
        let err = parse_args_from(&raw).unwrap_err();
        assert!(format!("{err:#}").contains("--session is required"));
    }

    #[test]
    fn parse_session_missing_value() {
        let raw: Vec<String> = vec!["--session".into()];
        let err = parse_args_from(&raw).unwrap_err();
        assert!(format!("{err:#}").contains("--session requires a value"));
    }

    #[test]
    fn parse_unknown_arg() {
        let raw: Vec<String> = vec!["--session".into(), "s1".into(), "--foobar".into()];
        let err = parse_args_from(&raw).unwrap_err();
        assert!(format!("{err:#}").contains("unexpected argument"));
    }

    #[test]
    fn parse_default_workspace_is_current_dir() {
        let raw: Vec<String> = vec!["--session".into(), "s1".into()];
        let args = parse_args_from(&raw).unwrap();
        assert_eq!(
            args.workspace,
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        );
    }

    #[test]
    fn parse_rejects_removed_replace_flag() {
        let raw = vec![
            "--session".into(),
            "s1".into(),
            "--replace-divergent-tail".into(),
        ];
        let err = parse_args_from(&raw).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("--replace-divergent-tail has been removed"),
            "expected removal message, got: {msg}"
        );
    }
}
