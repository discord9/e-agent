use std::path::Path;

use anyhow::{Context, anyhow};
use serde::{Deserialize, Serialize};

use crate::agent::Message;

const VERSION: u32 = 1;

#[derive(Deserialize, Serialize)]
struct SavedSession {
    version: u32,
    messages: Vec<Message>,
}

pub struct Session;

impl Session {
    pub fn load(root: &Path, name: &str) -> anyhow::Result<Vec<Message>> {
        let path = session_path(root, name)?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let saved: SavedSession = serde_json::from_slice(
            &std::fs::read(&path)
                .with_context(|| format!("cannot read session {}", path.display()))?,
        )
        .with_context(|| format!("cannot decode session {}", path.display()))?;
        if saved.version != VERSION {
            anyhow::bail!("unsupported session version {}", saved.version);
        }
        Ok(saved.messages)
    }

    pub fn save(root: &Path, name: &str, messages: &[Message]) -> anyhow::Result<()> {
        let path = session_path(root, name)?;
        let directory = path.parent().unwrap();
        std::fs::create_dir_all(directory)
            .with_context(|| format!("cannot create session directory {}", directory.display()))?;
        let temporary = directory.join(format!(".{name}.{}.tmp", std::process::id()));
        let contents = serde_json::to_vec_pretty(&SavedSession {
            version: VERSION,
            messages: messages.to_vec(),
        })?;
        std::fs::write(&temporary, contents)
            .with_context(|| format!("cannot write session {}", temporary.display()))?;
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

fn session_path(root: &Path, name: &str) -> anyhow::Result<std::path::PathBuf> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(anyhow!("session name must contain only [a-zA-Z0-9_-]"));
    }
    Ok(root.join(".e-agent/sessions").join(format!("{name}.json")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Message;

    #[test]
    fn round_trips_messages_and_missing_session_is_empty() {
        let temp = tempfile::tempdir().unwrap();
        assert!(Session::load(temp.path(), "missing").unwrap().is_empty());
        let messages = vec![
            Message::User {
                content: "hello".into(),
            },
            Message::Assistant(crate::agent::AssistantMessage {
                content: Some("answer".into()),
                tool_calls: vec![],
                reasoning: Some("thinking".into()),
            }),
        ];
        Session::save(temp.path(), "work", &messages).unwrap();
        assert_eq!(Session::load(temp.path(), "work").unwrap(), messages);
    }

    #[test]
    fn rejects_invalid_names_and_versions() {
        let temp = tempfile::tempdir().unwrap();
        assert!(Session::save(temp.path(), "../bad", &[]).is_err());
        let path = temp.path().join(".e-agent/sessions/bad.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, r#"{"version":2,"messages":[]}"#).unwrap();
        assert!(
            Session::load(temp.path(), "bad")
                .unwrap_err()
                .to_string()
                .contains("unsupported session version")
        );
    }

    #[cfg(unix)]
    #[test]
    fn saves_private_session_files() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        Session::save(temp.path(), "private", &[]).unwrap();
        let mode = std::fs::metadata(temp.path().join(".e-agent/sessions/private.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
