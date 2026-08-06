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

/// Validate that a session name contains only [a-zA-Z0-9_-]. Shared by all
/// backends — JSONL files need this for file-system safety, and Greptime
/// sessions are persisted as JSONL background records and must therefore
/// pass the same check.
pub fn validate_session_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        anyhow::bail!("session name must contain only [a-zA-Z0-9_-]");
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
        let path = session_path(root, name, "jsonl")?;
        #[cfg(unix)]
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
}
