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
        let path = session_path(root, name, "jsonl")?;
        let created = !path.exists();
        let directory = path.parent().unwrap();
        std::fs::create_dir_all(directory)
            .with_context(|| format!("cannot create session directory {}", directory.display()))?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("cannot append session {}", path.display()))?;
        for entry in entries {
            file.write_all(&serde_json::to_vec(entry)?)?;
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
        #[cfg(unix)]
        if created {
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        }
        Ok(())
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
    pub fn record_background_start(
        root: &Path,
        session: &str,
        id: u64,
        label: &str,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let path = background_record_path(root, session)?;
        let directory = path.parent().unwrap();
        std::fs::create_dir_all(directory)?;
        let created = !path.exists();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let mut record = serde_json::json!({
            "id": id,
            "label": label,
        });
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
                match record["session_id"].as_str() {
                    Some(sid) if !sid.is_empty() => {
                        labels.push(format!("task {id}: {label} (session: {sid})"));
                    }
                    _ => labels.push(format!("task {id}: {label}")),
                }
            }
        }
        let _ = std::fs::remove_file(&path);
        labels
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Message;

    fn entries() -> Vec<SessionEntry> {
        vec![
            Message::User {
                content: "hello".into(),
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

        Session::record_background_start(temp.path(), "a", 1, "sleep 100", None).unwrap();
        Session::record_background_start(temp.path(), "a", 2, "cargo build", None).unwrap();
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
        Session::record_background_start(temp.path(), "a", 3, "x", None).unwrap();
        Session::clear_background_task(temp.path(), "a", 3);
        assert!(
            !temp
                .path()
                .join(".e-agent/sessions/a.background.jsonl")
                .exists()
        );
    }
}
