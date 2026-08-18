use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Context, anyhow};

use crate::agent::{Message, SessionEntry};

const LEGACY_VERSION: u32 = 1;

/// The old implicit session every TUI launch resumed. Migrated on startup
/// to a timestamped id so concurrent instances never share a file.
const LEGACY_DEFAULT_SESSION: &str = "default";

/// Generate a fresh session id: `YYYYMMDD-HHMMSS-xxxx` (local time plus a
/// random suffix). Sortable, human-readable, and unique across restarts —
/// unlike the old background-task counter, which restarted at 1.
pub fn new_id() -> String {
    format!("{}-{}", timestamp(), random_suffix())
}

/// Like [`new_id`], with a short prefix marking the session kind (e.g.
/// `sub-` for subagent sessions).
pub fn new_id_prefixed(prefix: &str) -> String {
    format!("{prefix}{}", new_id())
}

fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Civil date from unix days (Howard Hinnant's algorithm), no chrono.
    let days = (secs / 86_400) as i64;
    let seconds_of_day = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        seconds_of_day / 3600,
        (seconds_of_day / 60) % 60,
        seconds_of_day % 60
    )
}

fn random_suffix() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    // /dev/urandom for the suffix; fall back to time-mixing if unavailable.
    let mut bytes = [0u8; 4];
    let filled = std::fs::File::open("/dev/urandom")
        .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut bytes))
        .is_ok();
    if !filled {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        bytes = (nanos ^ std::process::id()).to_le_bytes();
    }
    bytes
        .iter()
        .map(|byte| ALPHABET[(byte % 32) as usize] as char)
        .collect()
}

/// Migrate pre-session-id files to unique ids:
///
/// - `default.jsonl` (the old implicit resume target) becomes
///   `default-<new id>.jsonl`; its content is preserved under the new id
///   and the old file is removed. Note the id is printed by the caller so
///   it can be resumed with `--session`.
/// - `subagent-<task-id>.jsonl` (task ids restart at 1 every process, so
///   these names are ambiguous across restarts) becomes
///   `sub-migrated-<new id>-<task-id>.jsonl`.
///
/// Explicitly named sessions (created via `--session foo`) are untouched:
/// their resume semantics are unchanged.
pub fn migrate_legacy(root: &Path) -> Vec<(String, String)> {
    let mut migrated = Vec::new();
    let directory = root.join(".e-agent/sessions");
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return migrated;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".jsonl") else {
            continue;
        };
        let new_name = if stem == LEGACY_DEFAULT_SESSION {
            Some(format!("{LEGACY_DEFAULT_SESSION}-{}", new_id()))
        } else {
            stem.strip_prefix("subagent-")
                .map(|task_id| format!("sub-migrated-{}-{}", new_id(), task_id))
        };
        if let Some(new_name) = new_name {
            let target = directory.join(format!("{new_name}.jsonl"));
            if std::fs::rename(entry.path(), &target).is_ok() {
                migrated.push((stem.to_owned(), new_name));
            }
        }
    }
    migrated
}

#[derive(serde::Deserialize, serde::Serialize)]
struct LegacySession {
    version: u32,
    messages: Vec<Message>,
}

#[derive(Debug)]
pub struct LoadedSession {
    pub entries: Vec<SessionEntry>,
    /// True when loaded from the old whole-document v1 format; the caller
    /// should rewrite the file as JSONL before appending.
    pub legacy: bool,
}

/// Validate that a session name contains only [a-zA-Z0-9_-] and is at
/// most [`crate::session_store::MAX_SESSION_ID_LEN`] chars. Shared by all
/// backends — JSONL files need this for file-system safety, Greptime
/// sessions are persisted as JSONL background records, and receipts bind
/// the session id (the length bound keeps receipts bounded).
pub fn validate_session_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty()
        || name.len() > crate::session_store::MAX_SESSION_ID_LEN
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        anyhow::bail!(
            "session name must be 1..={} chars of [a-zA-Z0-9_-]",
            crate::session_store::MAX_SESSION_ID_LEN
        );
    }
    Ok(())
}

pub struct Session;

impl Session {
    pub fn load(root: &Path, name: &str) -> anyhow::Result<LoadedSession> {
        let jsonl = session_path(root, name, "jsonl")?;
        if jsonl.exists() {
            let file = std::fs::File::open(&jsonl)
                .with_context(|| format!("cannot open session {}", jsonl.display()))?;
            let mut entries = Vec::new();
            for (index, line) in std::io::BufReader::new(file).lines().enumerate() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                entries.push(
                    serde_json::from_str::<SessionEntry>(&line).with_context(|| {
                        format!(
                            "cannot decode session {} line {}",
                            jsonl.display(),
                            index + 1
                        )
                    })?,
                );
            }
            return Ok(LoadedSession {
                entries,
                legacy: false,
            });
        }
        let legacy = session_path(root, name, "json")?;
        if !legacy.exists() {
            return Ok(LoadedSession {
                entries: Vec::new(),
                legacy: false,
            });
        }
        let saved: LegacySession = serde_json::from_slice(
            &std::fs::read(&legacy)
                .with_context(|| format!("cannot read session {}", legacy.display()))?,
        )
        .with_context(|| format!("cannot decode session {}", legacy.display()))?;
        if saved.version != LEGACY_VERSION {
            anyhow::bail!("unsupported session version {}", saved.version);
        }
        Ok(LoadedSession {
            entries: saved.messages.into_iter().map(SessionEntry::from).collect(),
            legacy: true,
        })
    }

    /// Append new entries to the JSONL log, creating the file (0600) if needed.
    pub fn append(root: &Path, name: &str, entries: &[SessionEntry]) -> anyhow::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        append_batch_locked(root, name, entries).map(|_| ())
    }

    /// Append new entries to the JSONL log, returning the exact physical
    /// located key of each appended line: the 0-based ordinal of the line
    /// (the append-only file's line count before this write) plus the
    /// SHA-256 of the exact serialized payload bytes. The whole
    /// count+serialize+append+sync+location window runs under ONE
    /// exclusive lock on the session file (cross-process advisory flock on
    /// Unix), so the ordinals are the file's true physical positions even
    /// with concurrent writers.
    pub fn append_located(
        root: &Path,
        name: &str,
        entries: &[SessionEntry],
    ) -> anyhow::Result<Vec<crate::session_store::EntryLocation>> {
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        let (base, payloads) = append_batch_locked(root, name, entries)?;
        let fingerprint = crate::session_store::workspace_root_fingerprint(root);
        let backend_fp = crate::session_store::backend_instance_fingerprint(
            "jsonl",
            &crate::session_store::derive_workspace_id(root),
        );
        Ok(payloads
            .into_iter()
            .enumerate()
            .map(|(i, payload)| {
                let payload = String::from_utf8(payload).expect("serialized JSON is valid UTF-8");
                crate::session_store::EntryLocation {
                    backend: "jsonl",
                    fingerprint: fingerprint.clone(),
                    backend_fp: backend_fp.clone(),
                    session: name.to_owned(),
                    key: crate::session_store::LocatedKey::Jsonl {
                        ordinal: base as i64 + i as i64,
                    },
                    entry_hash: crate::session_store::entry_payload_hash(&payload),
                }
            })
            .collect())
    }

    /// Load the full JSONL history paired with each line's exact physical
    /// location (ordinal + payload hash). Legacy whole-document `.json`
    /// sessions load with `None` locations (they are rewritten to JSONL by
    /// the caller; until then no receipt refs are issued for them).
    pub fn load_located(
        root: &Path,
        name: &str,
    ) -> anyhow::Result<crate::session_store::LoadedLocated> {
        let jsonl = session_path(root, name, "jsonl")?;
        if jsonl.exists() {
            let file = std::fs::File::open(&jsonl)
                .with_context(|| format!("cannot open session {}", jsonl.display()))?;
            let fingerprint = crate::session_store::workspace_root_fingerprint(root);
            let mut entries = Vec::new();
            let mut locations = Vec::new();
            for (index, line) in std::io::BufReader::new(file).lines().enumerate() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let entry = serde_json::from_str::<SessionEntry>(&line).with_context(|| {
                    format!(
                        "cannot decode session {} line {}",
                        jsonl.display(),
                        index + 1
                    )
                })?;
                locations.push(Some(crate::session_store::EntryLocation {
                    backend: "jsonl",
                    fingerprint: fingerprint.clone(),
                    backend_fp: crate::session_store::backend_instance_fingerprint(
                        "jsonl",
                        &crate::session_store::derive_workspace_id(root),
                    ),
                    session: name.to_owned(),
                    key: crate::session_store::LocatedKey::Jsonl {
                        ordinal: entries.len() as i64,
                    },
                    entry_hash: crate::session_store::entry_payload_hash(&line),
                }));
                entries.push(entry);
            }
            return Ok(crate::session_store::LoadedLocated {
                entries,
                locations,
                legacy: false,
            });
        }
        let legacy = session_path(root, name, "json")?;
        if !legacy.exists() {
            return Ok(crate::session_store::LoadedLocated {
                entries: Vec::new(),
                locations: Vec::new(),
                legacy: false,
            });
        }
        let saved: LegacySession = serde_json::from_slice(
            &std::fs::read(&legacy)
                .with_context(|| format!("cannot read session {}", legacy.display()))?,
        )
        .with_context(|| format!("cannot decode session {}", legacy.display()))?;
        if saved.version != LEGACY_VERSION {
            anyhow::bail!("unsupported session version {}", saved.version);
        }
        let entries: Vec<SessionEntry> =
            saved.messages.into_iter().map(SessionEntry::from).collect();
        Ok(crate::session_store::LoadedLocated {
            locations: vec![None; entries.len()],
            entries,
            legacy: true,
        })
    }

    /// Exact-version field read on the JSONL backend: read the exact line
    /// at the receipt's ordinal, verify its payload hash (a rewritten file
    /// or a retargeted ordinal fails with `integrity`), extract the field,
    /// and reject total-size drift.
    pub fn read_field(
        root: &Path,
        verified: &crate::output_receipt::VerifiedRef,
    ) -> anyhow::Result<Vec<u8>> {
        use crate::output_receipt::{ReceiptError, ReceiptErrorKind};
        let location = &verified.location;
        if location.backend != "jsonl" {
            return Err(ReceiptError::new(
                ReceiptErrorKind::Integrity,
                "receipt backend does not match this store",
            )
            .into());
        }
        if location.fingerprint != crate::session_store::workspace_root_fingerprint(root) {
            return Err(ReceiptError::new(
                ReceiptErrorKind::Integrity,
                "receipt is for a different workspace",
            )
            .into());
        }
        if location.backend_fp
            != crate::session_store::backend_instance_fingerprint(
                "jsonl",
                &crate::session_store::derive_workspace_id(root),
            )
        {
            return Err(ReceiptError::new(
                ReceiptErrorKind::Integrity,
                "receipt is for a different backend instance",
            )
            .into());
        }
        let path = session_path(root, &location.session, "jsonl")?;
        if !path.exists() {
            return Err(ReceiptError::new(
                ReceiptErrorKind::Integrity,
                "session file no longer exists",
            )
            .into());
        }
        let crate::session_store::LocatedKey::Jsonl { ordinal } = location.key else {
            unreachable!("backend checked above");
        };
        let file = std::fs::File::open(&path)
            .with_context(|| format!("cannot open session {}", path.display()))?;
        let mut found: Option<String> = None;
        let mut line_index = 0i64;
        for line in std::io::BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if line_index == ordinal {
                found = Some(line);
                break;
            }
            line_index += 1;
        }
        let Some(line) = found else {
            return Err(ReceiptError::new(
                ReceiptErrorKind::Integrity,
                format!("entry not found at pinned ordinal {ordinal}"),
            )
            .into());
        };
        if crate::session_store::entry_payload_hash(&line) != location.entry_hash {
            return Err(ReceiptError::new(
                ReceiptErrorKind::Integrity,
                "entry changed since the receipt was issued",
            )
            .into());
        }
        let entry: SessionEntry = serde_json::from_str(&line).map_err(|error| {
            ReceiptError::new(
                ReceiptErrorKind::Integrity,
                format!("cannot decode pinned entry: {error}"),
            )
        })?;
        let bytes =
            crate::output_receipt::field_bytes(&entry, verified.field).ok_or_else(|| {
                ReceiptError::new(
                    ReceiptErrorKind::Integrity,
                    "pinned entry no longer carries the receipt's field",
                )
            })?;
        if bytes.len() != verified.total {
            return Err(ReceiptError::new(
                ReceiptErrorKind::Integrity,
                format!(
                    "field size changed since the receipt was issued ({} != {})",
                    bytes.len(),
                    verified.total
                ),
            )
            .into());
        }
        Ok(bytes)
    }

    /// Direct current-session field read by durable JSONL nonblank ordinal.
    pub fn read_field_direct(
        root: &Path,
        name: &str,
        ordinal: i64,
        field: crate::output_receipt::FieldId,
    ) -> anyhow::Result<Vec<u8>> {
        use crate::output_receipt::{ReceiptError, ReceiptErrorKind};
        if ordinal < 0 {
            return Err(ReceiptError::new(ReceiptErrorKind::Invalid, "invalid entry id").into());
        }
        let path = session_path(root, name, "jsonl")?;
        let file = std::fs::File::open(&path)
            .with_context(|| format!("cannot open session {}", path.display()))?;
        let entry = std::io::BufReader::new(file)
            .lines()
            .map_while(|line| line.ok())
            .filter(|line| !line.trim().is_empty())
            .nth(ordinal as usize)
            .ok_or_else(|| ReceiptError::new(ReceiptErrorKind::Unavailable, "entry not found"))?;
        let entry: SessionEntry = serde_json::from_str(&entry).map_err(|error| {
            ReceiptError::new(
                ReceiptErrorKind::Unavailable,
                format!("cannot decode entry: {error}"),
            )
        })?;
        crate::output_receipt::field_bytes(&entry, field).ok_or_else(|| {
            ReceiptError::new(
                ReceiptErrorKind::Unavailable,
                "entry does not carry that field",
            )
            .into()
        })
    }

    /// Rewrite the whole log (used once to migrate legacy sessions).
    pub fn rewrite(root: &Path, name: &str, entries: &[SessionEntry]) -> anyhow::Result<()> {
        let path = session_path(root, name, "jsonl")?;
        let directory = path.parent().unwrap();
        std::fs::create_dir_all(directory)
            .with_context(|| format!("cannot create session directory {}", directory.display()))?;
        let temporary = directory.join(format!(".{name}.{}.tmp", std::process::id()));
        {
            let mut file = std::fs::File::create(&temporary)
                .with_context(|| format!("cannot write session {}", temporary.display()))?;
            for entry in entries {
                file.write_all(&serde_json::to_vec(entry)?)?;
                file.write_all(b"\n")?;
            }
            file.sync_all()?;
        }
        #[cfg(unix)]
        std::fs::set_permissions(
            &temporary,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )?;
        std::fs::rename(&temporary, &path)
            .with_context(|| format!("cannot replace session {}", path.display()))?;
        Ok(())
    }
    /// Record a freshly started background task so a later launch can tell
    /// the user what died with the previous process. One JSON line per
    /// task; `clear_background_task` removes the line on completion, so a
    /// surviving line always means "the process died with this running".
    /// Records are scoped per session: a task started in session A is only
    /// reported back when session A is resumed, never to another session.
    ///
    /// `label` 是源头截断的 100 字符预览（给「被杀」Notice 用）；`full_command`
    /// 是完整命令原文（bash 任务传 `Some`，delegate 任务无命令传 `None`），
    /// 持久化在 JSONL 的 `"full_command"` 字段里，旧记录没有该字段 → 读回
    /// `None`（兼容）。`take_unfinished_background` 消费时仍只回 label
    /// （Notice 文本不变），完整命令通过 [`Self::task_full_command`] 单独取。
    pub fn record_background_start(
        root: &Path,
        session: &str,
        id: u64,
        label: &str,
        full_command: Option<&str>,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let path = background_record_path(root, session)?;
        let directory = path.parent().unwrap();
        std::fs::create_dir_all(directory)?;
        #[cfg(unix)]
        let created = !path.exists();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let mut record = serde_json::json!({
            "id": id,
            "label": label,
            "owner": crate::session_store::process_identity(),
        });
        if let Some(command) = full_command {
            record["full_command"] = serde_json::json!(command);
        }
        if let Some(sid) = session_id {
            record["session_id"] = serde_json::json!(sid);
        }
        file.write_all(&serde_json::to_vec(&record)?)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        #[cfg(unix)]
        if created {
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        }
        Ok(())
    }

    /// Forget one task: its completion arrived while the process was alive.
    pub fn clear_background_task(root: &Path, session: &str, id: u64) {
        let Ok(path) = background_record_path(root, session) else {
            return;
        };
        let Ok(file) = std::fs::File::open(&path) else {
            return;
        };
        let mut kept = Vec::new();
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            let matches = serde_json::from_str::<serde_json::Value>(&line)
                .ok()
                .and_then(|record| record["id"].as_u64())
                == Some(id);
            if !matches && !line.trim().is_empty() {
                kept.push(line);
            }
        }
        if kept.is_empty() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        if let Ok(mut file) = std::fs::File::create(&path) {
            for line in kept {
                let _ = writeln!(file, "{line}");
            }
            let _ = file.sync_all();
        }
    }

    /// Tasks recorded by a previous process that died before their
    /// completion arrived. Consumes the file; the caller injects the
    /// returned labels into the new session so the model can react
    /// (re-run the commands, apologize, ...). Only this session's own
    /// records are returned; other sessions' files are untouched.
    pub fn take_unfinished_background(root: &Path, session: &str) -> Vec<String> {
        let Ok(path) = background_record_path(root, session) else {
            return Vec::new();
        };
        let Ok(file) = std::fs::File::open(&path) else {
            return Vec::new();
        };
        let mut labels = Vec::new();
        for line in std::io::BufReader::new(file).lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(record) = serde_json::from_str::<serde_json::Value>(&line) {
                let id = record["id"].as_u64().unwrap_or(0);
                let label = record["label"].as_str().unwrap_or("?");
                let sid = record["session_id"].as_str();
                labels.push(format_unfinished(id, label, sid));
            }
        }
        let _ = std::fs::remove_file(&path);
        labels
    }

    /// Look up one surviving background-task record's full command by id
    /// (the "另取" path promised by [`Self::record_background_start`]: the
    /// consuming `take_unfinished_background` returns labels unchanged,
    /// this reads the persisted `"full_command"` separately). Old records
    /// written before the field shipped have no `full_command` → `None`
    /// (nothing to fill). Used by the server's `/api/tasks` fallback when
    /// the live registry lacks a full command.
    pub fn task_full_command(root: &Path, session: &str, task_id: u64) -> Option<String> {
        let Ok(path) = background_record_path(root, session) else {
            return None;
        };
        let Ok(file) = std::fs::File::open(&path) else {
            return None;
        };
        for line in std::io::BufReader::new(file).lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(record) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if record["id"].as_u64() == Some(task_id) {
                return record["full_command"].as_str().map(str::to_owned);
            }
        }
        None
    }

    /// True when every unfinished-task record for `session` was left by a
    /// now-dead process — i.e. the tasks really were killed with their
    /// owning process, so the caller may safely consume the records and
    /// inject the "killed with the process" notice (the TUI/CLI restart
    /// behavior; the server attaches lazily and may find a session that is
    /// still live in another process, which is why it probes first).
    ///
    /// Conservative: any uncertainty reports false ("not all dead"), so
    /// the caller keeps `Preserve` and never injects a false notice —
    /// a record without an `owner` field (written by an older version), an
    /// unparsable line, a live owner, an unreachable probe, or a read
    /// error all count as alive. Only a record file where EVERY line
    /// carries an owner that is definitely dead returns true. A
    /// missing/unreadable file returns true: no records means nothing to
    /// consume (the Consume path is a no-op).
    pub fn unfinished_owner_all_dead(root: &Path, session: &str) -> bool {
        let Ok(path) = background_record_path(root, session) else {
            return true;
        };
        let Ok(file) = std::fs::File::open(&path) else {
            return true;
        };
        for line in std::io::BufReader::new(file).lines() {
            let Ok(line) = line else { return false }; // read error: cannot judge
            if line.trim().is_empty() {
                continue;
            }
            let Ok(record) = serde_json::from_str::<serde_json::Value>(&line) else {
                return false; // unparsable line: cannot judge
            };
            match record["owner"].as_str() {
                None => return false, // old-format line without owner: alive
                Some(owner) => {
                    if crate::session_store::owner_alive(owner) {
                        return false; // owner still alive
                    }
                }
            }
        }
        // No records, or every record's owner is dead.
        true
    }
}

/// Render one unfinished-task record for the "killed on exit" notice,
/// shared by the JSONL file backend and the GreptimeDB `running_tasks`
/// table so both report tasks identically.
pub(crate) fn format_unfinished(
    task_id: u64,
    label: &str,
    subagent_session_id: Option<&str>,
) -> String {
    match subagent_session_id {
        Some(sid) if !sid.is_empty() => {
            format!("task {task_id}: {label} (session: {sid})")
        }
        _ => format!("task {task_id}: {label}"),
    }
}

fn background_record_path(root: &Path, session: &str) -> anyhow::Result<std::path::PathBuf> {
    if session.is_empty()
        || !session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        anyhow::bail!("invalid session name for background record: {session:?}");
    }
    Ok(root
        .join(".e-agent/sessions")
        .join(format!("{session}.background.jsonl")))
}

fn session_path(root: &Path, name: &str, extension: &str) -> anyhow::Result<std::path::PathBuf> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(anyhow!("session name must contain only [a-zA-Z0-9_-]"));
    }
    Ok(root
        .join(".e-agent/sessions")
        .join(format!("{name}.{extension}")))
}

/// Take the exclusive cross-process append lock on a JSONL session file
/// (advisory `flock` on Unix). The lock is released on drop or process
/// death, so a crashed writer can never leave a stale lock behind. Held
/// for the whole count+serialize+append+sync+location window so the
/// counted ordinal base is the true physical position even with
/// concurrent writers.
#[cfg(unix)]
fn lock_session_append(file: &std::fs::File, path: &Path) -> anyhow::Result<()> {
    rustix::fs::flock(file, rustix::fs::FlockOperation::LockExclusive)
        .with_context(|| format!("cannot lock session file {}", path.display()))
}

#[cfg(not(unix))]
fn lock_session_append(_file: &std::fs::File, _path: &Path) -> anyhow::Result<()> {
    // No flock on non-Unix platforms: the count-then-append window is
    // serialized only within this process. Cross-process JSONL appends on
    // Windows remain best-effort (documented limitation of the legacy
    // file backend).
    Ok(())
}

/// Serialize + append one batch to a JSONL session file under ONE
/// exclusive lock (see [`lock_session_append`]), returning the base
/// ordinal (the file's line count before this batch) and the exact
/// serialized payload bytes. Shared by [`Session::append`] and
/// [`Session::append_located`], so every JSONL append path holds the same
/// lock window and a concurrent `append` can never shift the ordinals
/// another writer counts.
fn append_batch_locked(
    root: &Path,
    name: &str,
    entries: &[SessionEntry],
) -> anyhow::Result<(usize, Vec<Vec<u8>>)> {
    let path = session_path(root, name, "jsonl")?;
    let directory = path.parent().unwrap();
    std::fs::create_dir_all(directory)
        .with_context(|| format!("cannot create session directory {}", directory.display()))?;
    #[cfg(unix)]
    let created = !path.exists();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(&path)
        .with_context(|| format!("cannot append session {}", path.display()))?;
    lock_session_append(&file, &path)?;
    // Count through the locked fd: the count is the ordinal base of the
    // lines this batch appends. After the count the fd is at EOF and
    // writes go to the physical end (O_APPEND).
    let base = {
        let mut reader = std::io::BufReader::new(&file);
        let mut count = 0usize;
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            let read = reader.read_until(b'\n', &mut buffer)?;
            if read == 0 {
                break;
            }
            if !buffer.iter().all(u8::is_ascii_whitespace) {
                count += 1;
            }
        }
        count
    };
    let mut payloads = Vec::with_capacity(entries.len());
    for entry in entries {
        let payload = serde_json::to_vec(entry)?;
        file.write_all(&payload)?;
        file.write_all(b"\n")?;
        payloads.push(payload);
    }
    file.sync_all()?;
    #[cfg(unix)]
    if created {
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    }
    Ok((base, payloads))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Message;

    fn entries() -> Vec<SessionEntry> {
        vec![
            Message::User {
                content: "hello".into(),
                images: vec![],
            }
            .into(),
            Message::Assistant(crate::agent::AssistantMessage {
                content: Some("answer".into()),
                tool_calls: vec![],
                reasoning: Some("thinking".into()),
            })
            .into(),
        ]
    }

    #[test]
    fn appends_and_loads_jsonl() {
        let temp = tempfile::tempdir().unwrap();
        let all = entries();
        Session::append(temp.path(), "work", &all[..1]).unwrap();
        Session::append(temp.path(), "work", &all[1..]).unwrap();
        let raw =
            std::fs::read_to_string(temp.path().join(".e-agent/sessions/work.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 2);
        let loaded = Session::load(temp.path(), "work").unwrap();
        assert!(!loaded.legacy);
        assert_eq!(loaded.entries, all);
    }

    #[cfg(unix)]
    #[test]
    fn saves_private_session_files() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        Session::append(temp.path(), "private", &entries()).unwrap();
        let mode = std::fs::metadata(temp.path().join(".e-agent/sessions/private.jsonl"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn fork_roundtrip_writes_marker_plus_prefix_and_leaves_source_untouched() {
        use crate::agent::fork_prefix;

        let temp = tempfile::tempdir().unwrap();
        let source = "source-session";
        // Two completed turns plus a trailing notice (not a turn boundary).
        let mut history = entries();
        history.extend(entries());
        history.push(SessionEntry::Notice {
            text: "tail notice".into(),
        });
        Session::append(temp.path(), source, &history).unwrap();

        let loaded = Session::load(temp.path(), source).unwrap();
        let prefix = fork_prefix(&loaded.entries, None).unwrap();
        assert_eq!(prefix.len(), 4, "trailing notice must be dropped");
        let marker = SessionEntry::ForkedFrom {
            source: source.into(),
            at: prefix.len(),
            event_time: None,
            seq: Some(prefix.len() as i64 - 1),
        };
        let mut fork_entries = vec![marker];
        fork_entries.extend(prefix);
        let forked = "forked-session";
        Session::rewrite(temp.path(), forked, &fork_entries).unwrap();

        // Source session is byte-for-byte unchanged.
        assert_eq!(Session::load(temp.path(), source).unwrap().entries, history);
        // The new session file is exactly [ForkedFrom] + prefix, and loads
        // back as the same entries (restore_history consumes this shape).
        let raw =
            std::fs::read_to_string(temp.path().join(".e-agent/sessions/forked-session.jsonl"))
                .unwrap();
        assert_eq!(raw.lines().count(), fork_entries.len());
        assert!(
            raw.lines()
                .next()
                .unwrap()
                .contains(r#""type":"forked_from""#),
            "first line must be the marker: {}",
            raw.lines().next().unwrap()
        );
        let reloaded = Session::load(temp.path(), forked).unwrap();
        assert!(!reloaded.legacy);
        assert_eq!(reloaded.entries, fork_entries);
    }

    #[test]
    fn loads_legacy_v1_and_rewrites_as_jsonl() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join(".e-agent/sessions");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("old.json"),
            r#"{"version":1,"messages":[{"User":{"content":"hi"}}]}"#,
        )
        .unwrap();
        std::fs::write(directory.join("bad.json"), r#"{"version":2,"messages":[]}"#).unwrap();

        let loaded = Session::load(temp.path(), "old").unwrap();
        assert!(loaded.legacy);
        assert_eq!(loaded.entries.len(), 1);
        assert!(
            Session::load(temp.path(), "bad")
                .unwrap_err()
                .to_string()
                .contains("unsupported session version")
        );

        Session::rewrite(temp.path(), "old", &loaded.entries).unwrap();
        let migrated = Session::load(temp.path(), "old").unwrap();
        assert!(!migrated.legacy);
        assert_eq!(migrated.entries, loaded.entries);
    }

    #[test]
    fn rejects_invalid_names() {
        let temp = tempfile::tempdir().unwrap();
        assert!(Session::append(temp.path(), "../bad", &entries()).is_err());
    }

    #[test]
    fn new_ids_are_unique_sortable_and_valid_session_names() {
        let a = new_id();
        let b = new_id_prefixed("sub-");
        assert_ne!(a, b);
        // YYYYMMDD-HHMMSS-xxxx
        assert_eq!(a.len(), 8 + 1 + 6 + 1 + 4, "{a}");
        assert!(b.starts_with("sub-"));
        // Valid session names (file-system safe).
        Session::append(tempfile::tempdir().unwrap().path(), &a, &entries()[..1]).unwrap();
        // Timestamp prefix makes ids sortable by creation time (within the
        // same second the random suffix decides, so only the date part is
        // asserted here).
        let later = new_id();
        assert!(later[..15] >= a[..15]);
    }

    #[test]
    fn migrates_default_and_task_id_subagent_files_only() {
        let temp = tempfile::tempdir().unwrap();
        Session::append(temp.path(), "default", &entries()[..1]).unwrap();
        Session::append(temp.path(), "subagent-3", &entries()[..1]).unwrap();
        Session::append(temp.path(), "work", &entries()[..1]).unwrap();
        let migrated = migrate_legacy(temp.path());
        assert_eq!(migrated.len(), 2);
        let default_new = migrated
            .iter()
            .find(|(old, _)| old == "default")
            .map(|(_, new)| new.clone())
            .unwrap();
        assert!(default_new.starts_with("default-"));
        let sub_new = migrated
            .iter()
            .find(|(old, _)| old == "subagent-3")
            .map(|(_, new)| new.clone())
            .unwrap();
        assert!(sub_new.starts_with("sub-migrated-"));
        assert!(sub_new.ends_with("-3"));
        // Content preserved under the new ids; old files gone.
        assert_eq!(
            Session::load(temp.path(), &default_new).unwrap().entries,
            entries()[..1]
        );
        assert_eq!(
            Session::load(temp.path(), &sub_new).unwrap().entries,
            entries()[..1]
        );
        assert!(
            Session::load(temp.path(), "default")
                .unwrap()
                .entries
                .is_empty()
        );
        // Named sessions untouched; migration is idempotent.
        assert_eq!(Session::load(temp.path(), "work").unwrap().entries.len(), 1);
        assert!(migrate_legacy(temp.path()).is_empty());
    }

    #[test]
    fn background_record_lifecycle() {
        let temp = tempfile::tempdir().unwrap();
        // Nothing recorded: nothing to report.
        assert!(Session::take_unfinished_background(temp.path(), "a").is_empty());

        Session::record_background_start(temp.path(), "a", 1, "sleep 100", None, None).unwrap();
        Session::record_background_start(temp.path(), "a", 2, "cargo build", None, None).unwrap();
        // Records are scoped per session: session b sees none of a's tasks.
        assert!(Session::take_unfinished_background(temp.path(), "b").is_empty());
        // Task 1 completes while we are alive: only task 2 stays on record.
        Session::clear_background_task(temp.path(), "a", 1);
        assert_eq!(
            Session::take_unfinished_background(temp.path(), "a"),
            vec!["task 2: cargo build".to_string()]
        );
        // take consumes the file: a second launch has nothing to report.
        assert!(Session::take_unfinished_background(temp.path(), "a").is_empty());
        // Clearing the last recorded task removes the file entirely.
        Session::record_background_start(temp.path(), "a", 3, "x", None, None).unwrap();
        Session::clear_background_task(temp.path(), "a", 3);
        assert!(
            !temp
                .path()
                .join(".e-agent/sessions/a.background.jsonl")
                .exists()
        );
    }

    /// Every recorded task line carries the recording process's identity
    /// (`pid@hostname#nonce`), so a later launch can probe whether the
    /// owner died with the task.
    #[test]
    fn background_record_carries_owner_identity() {
        let temp = tempfile::tempdir().unwrap();
        Session::record_background_start(temp.path(), "owner-test", 1, "sleep 100", None, None)
            .unwrap();
        let raw = std::fs::read_to_string(
            temp.path()
                .join(".e-agent/sessions/owner-test.background.jsonl"),
        )
        .unwrap();
        let record: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(
            record["owner"].as_str(),
            Some(crate::session_store::process_identity()),
            "record must carry the recording process identity"
        );
        // The label/id round-trip is unchanged by the new field.
        assert_eq!(record["id"].as_u64(), Some(1));
        assert_eq!(record["label"].as_str(), Some("sleep 100"));
        assert_eq!(
            Session::take_unfinished_background(temp.path(), "owner-test"),
            vec!["task 1: sleep 100".to_string()]
        );
    }

    /// `record_background_start` persists the FULL command (not just the
    /// truncated label) under `"full_command"`; `task_full_command` reads
    /// it back by id. Consuming (`take_unfinished_background`) still
    /// returns only labels, so the "killed on exit" notice text is
    /// unchanged; the full command stays retrievable separately until the
    /// record is consumed.
    #[test]
    fn background_record_persists_full_command() {
        let temp = tempfile::tempdir().unwrap();
        let long = "cargo build --release --features very-long-feature-name --target x86_64-unknown-linux-gnu";
        Session::record_background_start(
            temp.path(),
            "cmd-store",
            1,
            "cargo build …",
            Some(long),
            None,
        )
        .unwrap();
        // Delegate-style record: no command.
        Session::record_background_start(temp.path(), "cmd-store", 2, "delegate work", None, None)
            .unwrap();

        // The raw JSON line carries the full command verbatim.
        let raw = std::fs::read_to_string(
            temp.path()
                .join(".e-agent/sessions/cmd-store.background.jsonl"),
        )
        .unwrap();
        let lines: Vec<serde_json::Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["full_command"].as_str(), Some(long));
        assert!(
            lines[1].get("full_command").is_none(),
            "delegate record has no command"
        );

        // Lookup by id returns the persisted full command; a missing id or
        // a record without the field returns None.
        assert_eq!(
            Session::task_full_command(temp.path(), "cmd-store", 1).as_deref(),
            Some(long)
        );
        assert_eq!(
            Session::task_full_command(temp.path(), "cmd-store", 2),
            None
        );
        assert_eq!(
            Session::task_full_command(temp.path(), "cmd-store", 99),
            None
        );

        // Old-format records (no `full_command` field) read back as None —
        // backwards compatible with records written before the field shipped.
        let dir = temp.path().join(".e-agent/sessions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("legacy.background.jsonl"),
            "{\"id\":5,\"label\":\"old task\",\"owner\":\"x@y#z\"}\n",
        )
        .unwrap();
        assert_eq!(Session::task_full_command(temp.path(), "legacy", 5), None);

        // The consumption path is unchanged: labels only.
        assert_eq!(
            Session::take_unfinished_background(temp.path(), "cmd-store"),
            vec![
                "task 1: cargo build …".to_string(),
                "task 2: delegate work".to_string()
            ]
        );
        // Records consumed → lookup gone with the file.
        assert_eq!(
            Session::task_full_command(temp.path(), "cmd-store", 1),
            None
        );
    }

    /// `unfinished_owner_all_dead` is the server-attach probe: true only
    /// when EVERY record was left by a definitely-dead process. Missing
    /// file / no records → true (Consume is a no-op); a live owner, an
    /// old-format line without an owner, or a malformed line → false
    /// (conservative Preserve).
    #[test]
    fn unfinished_owner_all_dead_is_conservative() {
        let temp = tempfile::tempdir().unwrap();

        // No record file → nothing unfinished → all dead (vacuously).
        assert!(Session::unfinished_owner_all_dead(
            temp.path(),
            "dead-probe"
        ));

        // Records written by THIS process (still alive) → not all dead.
        Session::record_background_start(temp.path(), "dead-probe", 1, "sleep 100", None, None)
            .unwrap();
        assert!(!Session::unfinished_owner_all_dead(
            temp.path(),
            "dead-probe"
        ));

        // Rewrite the record with a definitely-dead owner → all dead.
        // Reachable only with an exported hostname: with none, the owner's
        // hostname falls back to "unknown" and the record is unjudgeable
        // → alive (P2-2), which the else-branch asserts instead.
        let path = temp
            .path()
            .join(".e-agent/sessions/dead-probe.background.jsonl");
        let probeable = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .ok()
            .filter(|h| !h.is_empty() && h != "unknown");
        match probeable {
            Some(hostname) => {
                let dead = format!("2000000000@{hostname}#deadbeef");
                std::fs::write(
                    &path,
                    format!("{{\"id\":1,\"label\":\"sleep 100\",\"owner\":\"{dead}\"}}\n"),
                )
                .unwrap();
                assert!(Session::unfinished_owner_all_dead(
                    temp.path(),
                    "dead-probe"
                ));

                // Mixed: one dead owner + one live owner → not all dead.
                std::fs::write(
                    &path,
                    format!(
                        "{{\"id\":1,\"label\":\"a\",\"owner\":\"{dead}\"}}\n{{\"id\":2,\"label\":\"b\",\"owner\":\"{}\"}}\n",
                        crate::session_store::process_identity()
                    ),
                )
                .unwrap();
                assert!(!Session::unfinished_owner_all_dead(
                    temp.path(),
                    "dead-probe"
                ));
            }
            None => {
                // No exported hostname: an "unknown"-hostname owner is
                // unjudgeable → alive → not all dead, even for a dead pid.
                std::fs::write(
                    &path,
                    "{\"id\":1,\"label\":\"sleep 100\",\"owner\":\"2000000000@unknown#deadbeef\"}\n",
                )
                .unwrap();
                assert!(!Session::unfinished_owner_all_dead(
                    temp.path(),
                    "dead-probe"
                ));
            }
        }

        // A foreign hostname owner cannot be judged → alive → false.
        let foreign = format!(
            "{{\"id\":1,\"label\":\"x\",\"owner\":\"{}@elsewhere#n\"}}\n",
            std::process::id()
        );
        std::fs::write(&path, foreign).unwrap();
        assert!(!Session::unfinished_owner_all_dead(
            temp.path(),
            "dead-probe"
        ));

        // Old-format line WITHOUT an owner field → treated as alive.
        std::fs::write(&path, "{\"id\":1,\"label\":\"old task\"}\n").unwrap();
        assert!(!Session::unfinished_owner_all_dead(
            temp.path(),
            "dead-probe"
        ));

        // Malformed JSON line → cannot judge → false.
        std::fs::write(&path, "not json\n").unwrap();
        assert!(!Session::unfinished_owner_all_dead(
            temp.path(),
            "dead-probe"
        ));

        // Empty file (all lines consumed by a racing clear) → true.
        std::fs::write(&path, "").unwrap();
        assert!(Session::unfinished_owner_all_dead(
            temp.path(),
            "dead-probe"
        ));
    }

    #[test]
    fn format_unfinished_includes_subagent_session_only_when_present() {
        assert_eq!(format_unfinished(1, "sleep 100", None), "task 1: sleep 100");
        assert_eq!(
            format_unfinished(2, "cargo build", Some("sub-abc")),
            "task 2: cargo build (session: sub-abc)"
        );
        // Empty subagent session id is treated as absent (JSONL never
        // writes an empty string, but be defensive).
        assert_eq!(format_unfinished(3, "x", Some("")), "task 3: x");
    }

    // ── Located keys: append_located / load_located / read_field ──────

    use crate::output_receipt::{
        FieldId, VerifiedRef, issue_legacy_for_test, page_field, verify_legacy_for_test,
    };
    use crate::session_store::{LoadedLocated, LocatedKey};

    fn located_entries() -> Vec<SessionEntry> {
        vec![
            Message::User {
                content: "first".into(),
                images: vec![],
            }
            .into(),
            SessionEntry::BackgroundCompletion {
                id: 1,
                output: "big-output".into(),
                label: None,
                started_at_ms: None,
                duration_ms: None,
                exit_code: None,
                signal: None,
                status: None,
                kind: None,
            },
        ]
    }

    #[test]
    fn append_located_returns_exact_ordinals_and_hashes() {
        let temp = tempfile::tempdir().unwrap();
        let entries = located_entries();
        let locations = Session::append_located(temp.path(), "loc", &entries).unwrap();
        assert_eq!(locations.len(), 2);
        assert_eq!(locations[0].backend, "jsonl");
        assert_eq!(locations[0].session, "loc");
        assert_eq!(locations[0].fingerprint.len(), 32);
        assert_eq!(locations[0].entry_hash.len(), 64);
        assert!(matches!(locations[0].key, LocatedKey::Jsonl { ordinal: 0 }));
        assert!(matches!(locations[1].key, LocatedKey::Jsonl { ordinal: 1 }));
        // The entry hash is the hash of the exact persisted line.
        let raw = std::fs::read_to_string(temp.path().join(".e-agent/sessions/loc.jsonl")).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            locations[0].entry_hash,
            crate::session_store::entry_payload_hash(lines[0])
        );
        // Appending more continues the ordinals (append-only file).
        let more = Session::append_located(temp.path(), "loc", &entries[..1]).unwrap();
        assert!(matches!(more[0].key, LocatedKey::Jsonl { ordinal: 2 }));
    }

    #[test]
    fn load_located_roundtrips_and_aligns_with_entries() {
        let temp = tempfile::tempdir().unwrap();
        let entries = located_entries();
        let locations = Session::append_located(temp.path(), "loc", &entries).unwrap();
        let loaded: LoadedLocated = Session::load_located(temp.path(), "loc").unwrap();
        assert!(!loaded.legacy);
        assert_eq!(loaded.entries, entries);
        assert_eq!(
            loaded.locations,
            locations.into_iter().map(Some).collect::<Vec<_>>()
        );
        // Unknown session → empty.
        let empty = Session::load_located(temp.path(), "nope").unwrap();
        assert!(empty.entries.is_empty());
        assert!(empty.locations.is_empty());
    }

    #[test]
    fn direct_field_read_survives_reconstructed_jsonl_session_and_stays_current_session() {
        let temp = tempfile::tempdir().unwrap();
        let one = vec![SessionEntry::Message {
            message: crate::agent::Message::Tool {
                call_id: "a".into(),
                name: "x".into(),
                content: "first".into(),
                images: vec![],
                is_error: false,
                synthetic: false,
            },
        }];
        let two = vec![SessionEntry::Message {
            message: crate::agent::Message::Tool {
                call_id: "b".into(),
                name: "x".into(),
                content: "second".into(),
                images: vec![],
                is_error: false,
                synthetic: false,
            },
        }];
        Session::append(temp.path(), "one", &one).unwrap();
        Session::append(temp.path(), "two", &two).unwrap();
        // A reconstructed context needs only root + current session + durable ordinal.
        assert_eq!(
            Session::read_field_direct(temp.path(), "one", 0, FieldId::ToolContent).unwrap(),
            b"first"
        );
        assert_eq!(
            Session::read_field_direct(temp.path(), "two", 0, FieldId::ToolContent).unwrap(),
            b"second"
        );
    }

    #[test]
    fn load_located_legacy_json_has_no_locations() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join(".e-agent/sessions");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("old.json"),
            r#"{"version":1,"messages":[{"User":{"content":"hi"}}]}"#,
        )
        .unwrap();
        let loaded = Session::load_located(temp.path(), "old").unwrap();
        assert!(loaded.legacy);
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.locations, vec![None]);
    }

    #[test]
    fn read_field_returns_exact_persisted_field_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let entries = located_entries();
        let locations = Session::append_located(temp.path(), "loc", &entries).unwrap();

        // User content.
        let verified = VerifiedRef {
            location: locations[0].clone(),
            field: FieldId::UserContent,
            total: "first".len(),
        };
        assert_eq!(
            Session::read_field(temp.path(), &verified).unwrap(),
            b"first"
        );

        // Background completion output.
        let verified = VerifiedRef {
            location: locations[1].clone(),
            field: FieldId::BgOutput,
            total: "big-output".len(),
        };
        assert_eq!(
            Session::read_field(temp.path(), &verified).unwrap(),
            b"big-output"
        );
        // Wrong field for the entry → integrity ("no longer carries").
        let wrong = VerifiedRef {
            location: locations[1].clone(),
            field: FieldId::ToolContent,
            total: 0,
        };
        let err = Session::read_field(temp.path(), &wrong).unwrap_err();
        assert!(
            format!("{err:#}").contains("integrity"),
            "wrong field must be an integrity error: {err:#}"
        );
    }

    #[test]
    fn read_field_rejects_hash_drift_and_missing_ordinal() {
        let temp = tempfile::tempdir().unwrap();
        let entries = located_entries();
        let locations = Session::append_located(temp.path(), "loc", &entries).unwrap();
        // Tampered entry hash → integrity (a rewritten file or a retargeted
        // ordinal can never satisfy an old ref).
        let mut tampered = locations[0].clone();
        tampered.entry_hash = "0".repeat(64);
        let verified = VerifiedRef {
            location: tampered,
            field: FieldId::UserContent,
            total: 5,
        };
        let err = Session::read_field(temp.path(), &verified).unwrap_err();
        assert!(format!("{err:#}").contains("integrity"), "{err:#}");

        // Ordinal past the end of the file → integrity.
        let mut missing = locations[0].clone();
        missing.key = LocatedKey::Jsonl { ordinal: 99 };
        let verified = VerifiedRef {
            location: missing,
            field: FieldId::UserContent,
            total: 5,
        };
        let err = Session::read_field(temp.path(), &verified).unwrap_err();
        assert!(format!("{err:#}").contains("integrity"), "{err:#}");

        // Wrong workspace fingerprint → integrity.
        let mut foreign = locations[0].clone();
        foreign.fingerprint = "1".repeat(32);
        let verified = VerifiedRef {
            location: foreign,
            field: FieldId::UserContent,
            total: 5,
        };
        let err = Session::read_field(temp.path(), &verified).unwrap_err();
        assert!(format!("{err:#}").contains("integrity"), "{err:#}");
    }

    #[test]
    fn read_field_total_drift_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let entries = located_entries();
        let locations = Session::append_located(temp.path(), "loc", &entries).unwrap();
        let verified = VerifiedRef {
            location: locations[0].clone(),
            field: FieldId::UserContent,
            total: 999, // wrong total
        };
        let err = Session::read_field(temp.path(), &verified).unwrap_err();
        assert!(
            format!("{err:#}").contains("size changed"),
            "total drift must be an integrity error: {err:#}"
        );
    }

    /// End-to-end page reconstruction: bound a field into a receipt,
    /// verify, read the exact full field, and page it with UTF-8-safe
    /// boundaries — the exact `read_output` contract.
    #[test]
    fn jsonl_page_reconstruction_with_receipt() {
        let temp = tempfile::tempdir().unwrap();
        let entries = located_entries();
        let locations = Session::append_located(temp.path(), "loc", &entries).unwrap();
        // Page the persisted "big-output" field.
        let receipt = issue_legacy_for_test(&locations[1], FieldId::BgOutput, "big-output".len());
        let verified = verify_legacy_for_test(&receipt).unwrap();
        let bytes = Session::read_field(temp.path(), &verified).unwrap();
        assert_eq!(bytes, b"big-output");
        let page = page_field(&bytes, 0, 2048).unwrap();
        assert_eq!(page.total_bytes, 10);
        assert_eq!(page.sha256.len(), 64);
        assert_eq!(page.text, "big-output");
        assert_eq!(page.next_offset, None);

        // A larger multibyte field: append it, page it in a chain, and
        // rebuild byte-exactly (UTF-8 seams never split chars).
        let output = format!("{}{}{}", "α".repeat(1000), "MIDDLE", "€".repeat(500));
        let entry = SessionEntry::BackgroundCompletion {
            id: 2,
            output: output.clone(),
            label: None,
            started_at_ms: None,
            duration_ms: None,
            exit_code: None,
            signal: None,
            status: None,
            kind: None,
        };
        let more = Session::append_located(temp.path(), "loc", &[entry]).unwrap();
        let receipt = issue_legacy_for_test(&more[0], FieldId::BgOutput, output.len());
        let verified = verify_legacy_for_test(&receipt).unwrap();
        let bytes = Session::read_field(temp.path(), &verified).unwrap();
        assert_eq!(bytes, output.as_bytes());
        let mut rebuilt = Vec::new();
        let mut offset = Some(0usize);
        while let Some(next) = offset {
            let page = page_field(&bytes, next, 1500).unwrap();
            rebuilt.extend_from_slice(page.text.as_bytes());
            offset = page.next_offset;
        }
        assert_eq!(rebuilt, output.as_bytes(), "paging chain must be lossless");
    }

    /// Session-name validation enforces the shared length bound too.
    #[test]
    fn validate_session_name_enforces_length_bound() {
        assert!(validate_session_name("ok-session_1").is_ok());
        let long = "x".repeat(crate::session_store::MAX_SESSION_ID_LEN);
        assert!(validate_session_name(&long).is_ok());
        let too_long = "x".repeat(crate::session_store::MAX_SESSION_ID_LEN + 1);
        assert!(validate_session_name(&too_long).is_err());
    }

    /// Concurrent writers (each with its own fd) must never interleave
    /// count and append: the returned ordinals are the file's true
    /// physical positions — contiguous, unique, and matching the loaded
    /// lines. The exclusive flock window covers
    /// count+serialize+append+sync+location construction.
    #[cfg(unix)]
    #[test]
    fn concurrent_appends_produce_contiguous_ordinals() {
        use std::sync::Arc;
        let root = tempfile::tempdir().unwrap();
        let session = "race-jsonl";
        let n = 8;
        let entries: Arc<Vec<SessionEntry>> = Arc::new(
            (0..n)
                .map(|i| {
                    Message::User {
                        content: format!("writer {i}"),
                        images: vec![],
                    }
                    .into()
                })
                .collect(),
        );
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let root = root.path().to_path_buf();
                let entries = entries.clone();
                std::thread::spawn(move || {
                    Session::append_located(&root, session, std::slice::from_ref(&entries[i]))
                        .unwrap()
                })
            })
            .collect();
        let mut ordinals: Vec<i64> = handles
            .into_iter()
            .flat_map(|handle| {
                handle
                    .join()
                    .unwrap()
                    .into_iter()
                    .map(|location| match location.key {
                        crate::session_store::LocatedKey::Jsonl { ordinal } => ordinal,
                        _ => panic!("jsonl location"),
                    })
            })
            .collect();
        ordinals.sort_unstable();
        assert_eq!(
            ordinals,
            (0..n as i64).collect::<Vec<i64>>(),
            "returned ordinals must be the true contiguous physical positions"
        );
        // The physical file agrees: n non-blank lines, aligned 1:1.
        let loaded = Session::load_located(root.path(), session).unwrap();
        assert_eq!(loaded.entries.len(), n);
        for (i, location) in loaded.locations.iter().enumerate() {
            match location {
                Some(crate::session_store::EntryLocation {
                    key: crate::session_store::LocatedKey::Jsonl { ordinal },
                    ..
                }) => assert_eq!(*ordinal, i as i64),
                other => panic!("every line must be located, got {other:?}"),
            }
        }
    }

    /// A receipt issued against one workspace root is rejected by
    /// `read_field` bound to a DIFFERENT root — the receipt's workspace
    /// (and, for uniformity, backend-instance) binding never resolves
    /// against another root.
    #[test]
    fn read_field_rejects_foreign_workspace_root() {
        use crate::output_receipt::{FieldId, issue_legacy_for_test, verify_legacy_for_test};
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let session = "sess";
        let entry = SessionEntry::BackgroundCompletion {
            id: 1,
            output: "out".into(),
            label: None,
            started_at_ms: None,
            duration_ms: None,
            exit_code: None,
            signal: None,
            status: None,
            kind: None,
        };
        let locations =
            Session::append_located(dir_a.path(), session, std::slice::from_ref(&entry)).unwrap();
        let receipt = issue_legacy_for_test(&locations[0], FieldId::BgOutput, 3);
        let verified = verify_legacy_for_test(&receipt).unwrap();
        // The owning root resolves; a different root is rejected with an
        // integrity binding error (different workspace / instance).
        assert_eq!(
            Session::read_field(dir_a.path(), &verified).unwrap(),
            b"out".to_vec()
        );
        let err = Session::read_field(dir_b.path(), &verified).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("different workspace") || text.contains("backend instance"),
            "{text}"
        );
    }
}
