use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Context, anyhow};

use crate::agent::{Message, SessionEntry};

const LEGACY_VERSION: u32 = 1;

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
}
