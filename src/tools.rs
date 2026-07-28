use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::agent::{Tool, ToolSpec};
use crate::workspace::Workspace;

const READ_LIMIT: usize = 64 * 1024;
const OUTPUT_LIMIT: usize = 64 * 1024;

pub fn builtins(workspace: Workspace) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(ReadFile {
            workspace: workspace.clone(),
        }),
        Box::new(WriteFile {
            workspace: workspace.clone(),
        }),
        Box::new(EditFile {
            workspace: workspace.clone(),
        }),
        Box::new(Bash {
            workspace,
            timeout: Duration::from_secs(30),
        }),
    ]
}

struct ReadFile {
    workspace: Workspace,
}

#[async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        spec(
            "read_file",
            "Read a UTF-8-ish file from the workspace.",
            json!({"path": {"type": "string", "description": "relative file path"}}),
        )
    }

    async fn execute(&self, arguments: Value) -> Result<String, String> {
        let bytes = self.workspace.read(required_string(&arguments, "path")?)?;
        let truncated = bytes.len() > READ_LIMIT;
        let mut text = String::from_utf8_lossy(&bytes[..bytes.len().min(READ_LIMIT)]).into_owned();
        if truncated {
            text.push_str("\n...[truncated]");
        }
        Ok(text)
    }
}

struct WriteFile {
    workspace: Workspace,
}

#[async_trait]
impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        spec(
            "write_file",
            "Write a file in the workspace, creating parent directories.",
            json!({
                "path": {"type": "string", "description": "relative file path"},
                "content": {"type": "string", "description": "file contents"}
            }),
        )
    }

    async fn execute(&self, arguments: Value) -> Result<String, String> {
        let content = required_string(&arguments, "content")?;
        self.workspace
            .write(required_string(&arguments, "path")?, content)?;
        Ok("file written".into())
    }
}

struct EditFile {
    workspace: Workspace,
}

#[async_trait]
impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        spec(
            "edit_file",
            "Replace exactly one literal occurrence in a workspace file.",
            json!({
                "path": {"type": "string", "description": "relative file path"},
                "old": {"type": "string", "description": "exact existing text"},
                "new": {"type": "string", "description": "replacement text"}
            }),
        )
    }

    async fn execute(&self, arguments: Value) -> Result<String, String> {
        let path = required_string(&arguments, "path")?;
        let old = required_string(&arguments, "old")?;
        let new = required_string(&arguments, "new")?;
        if old.is_empty() {
            return Err("`old` must not be empty".into());
        }
        let content = self.workspace.read_to_string(path)?;
        let matches = content.match_indices(old).count();
        if matches != 1 {
            return Err(format!(
                "expected `old` exactly once, found {matches} occurrences"
            ));
        }
        self.workspace.write(path, content.replacen(old, new, 1))?;
        Ok("file edited".into())
    }
}

struct Bash {
    workspace: Workspace,
    timeout: Duration,
}

#[async_trait]
impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        spec(
            "bash",
            "Run a shell command with the workspace as its current directory.",
            json!({"command": {"type": "string", "description": "shell command"}}),
        )
    }

    async fn execute(&self, arguments: Value) -> Result<String, String> {
        let command = required_string(&arguments, "command")?;
        let mut process = Command::new("/bin/bash");
        process
            .arg("-lc")
            .arg(command)
            .current_dir(self.workspace.root())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        process.process_group(0);
        let mut child = process
            .spawn()
            .map_err(|error| format!("failed to start shell: {error}"))?;
        #[cfg(unix)]
        let process_group = rustix::process::Pid::from_raw(
            child
                .id()
                .ok_or("bash exited before its process group was recorded")? as i32,
        )
        .ok_or("bash returned an invalid process id")?;
        let stdout = child.stdout.take().ok_or("failed to capture stdout")?;
        let stderr = child.stderr.take().ok_or("failed to capture stderr")?;
        let result = tokio::time::timeout(self.timeout, async {
            let (stdout, stderr, status) =
                tokio::join!(capture(stdout), capture(stderr), child.wait());
            Ok::<_, std::io::Error>((stdout?, stderr?, status?))
        })
        .await;
        let (stdout, stderr, status) = match result {
            Ok(result) => result.map_err(|error| format!("shell I/O failed: {error}"))?,
            Err(_) => {
                #[cfg(unix)]
                match rustix::process::kill_process_group(
                    process_group,
                    rustix::process::Signal::KILL,
                ) {
                    Ok(()) | Err(rustix::io::Errno::SRCH) => {}
                    Err(error) => {
                        return Err(format!("failed to kill bash process group: {error}"));
                    }
                }
                #[cfg(not(unix))]
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(format!(
                    "command timed out after {} seconds",
                    self.timeout.as_secs_f64()
                ));
            }
        };
        let text = format_output(status.code(), &stdout, &stderr);
        if status.success() {
            Ok(text)
        } else {
            Err(text)
        }
    }
}

fn spec(name: &str, description: &str, properties: Value) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        parameters: json!({"type": "object", "properties": properties, "required": properties.as_object().unwrap().keys().collect::<Vec<_>>() }),
    }
}

fn required_string<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, String> {
    arguments
        .as_object()
        .ok_or("tool arguments must be a JSON object")?
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("`{name}` must be a string"))
}

struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn capture(mut reader: impl AsyncRead + Unpin) -> std::io::Result<Captured> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(Captured { bytes, truncated });
        }
        let room = OUTPUT_LIMIT.saturating_sub(bytes.len());
        bytes.extend_from_slice(&buffer[..count.min(room)]);
        truncated |= count > room;
    }
}

fn format_output(code: Option<i32>, stdout: &Captured, stderr: &Captured) -> String {
    let mut output = format!(
        "exit code: {}\nstdout:\n{}\nstderr:\n{}",
        code.map_or_else(|| "signal".into(), |code| code.to_string()),
        String::from_utf8_lossy(&stdout.bytes),
        String::from_utf8_lossy(&stderr.bytes)
    );
    if stdout.truncated {
        output.push_str("\nstdout: [truncated]");
    }
    if stderr.truncated {
        output.push_str("\nstderr: [truncated]");
    }
    output
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::AsyncWriteExt;

    use super::*;

    async fn edit(temp: &tempfile::TempDir, old: &str, new: &str) -> Result<String, String> {
        EditFile {
            workspace: Workspace::new(temp.path()).unwrap(),
        }
        .execute(json!({"path": "file.txt", "old": old, "new": new}))
        .await
    }

    #[tokio::test]
    async fn edit_requires_exactly_one_match() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file.txt");
        std::fs::write(&path, "one two one").unwrap();
        assert!(
            edit(&temp, "missing", "x")
                .await
                .unwrap_err()
                .contains("found 0")
        );
        assert!(
            edit(&temp, "one", "x")
                .await
                .unwrap_err()
                .contains("found 2")
        );
        assert_eq!(edit(&temp, "two", "x").await.unwrap(), "file edited");
        assert_eq!(std::fs::read_to_string(path).unwrap(), "one x one");
    }

    #[tokio::test]
    async fn write_creates_a_new_nested_file() {
        let temp = tempfile::tempdir().unwrap();
        let tool = WriteFile {
            workspace: Workspace::new(temp.path()).unwrap(),
        };
        assert_eq!(
            tool.execute(json!({"path": "new/file.txt", "content": "hello"}))
                .await
                .unwrap(),
            "file written"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("new/file.txt")).unwrap(),
            "hello"
        );
    }

    #[tokio::test]
    async fn capture_drains_and_marks_each_truncated_stream() {
        async fn captured(bytes: Vec<u8>) -> Captured {
            let (mut writer, reader) = tokio::io::duplex(1024);
            tokio::spawn(async move { writer.write_all(&bytes).await.unwrap() });
            capture(reader).await.unwrap()
        }
        let stdout = captured(vec![b'o'; OUTPUT_LIMIT + 1]).await;
        let stderr = captured(vec![b'e'; OUTPUT_LIMIT + 1]).await;
        assert_eq!(stdout.bytes.len(), OUTPUT_LIMIT);
        assert_eq!(stderr.bytes.len(), OUTPUT_LIMIT);
        let output = format_output(Some(0), &stdout, &stderr);
        assert!(output.contains("stdout: [truncated]"));
        assert!(output.contains("stderr: [truncated]"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_timeout_kills_its_background_process_group() {
        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("child.pid");
        let tool = Bash {
            workspace: Workspace::new(temp.path()).unwrap(),
            timeout: Duration::from_millis(100),
        };
        assert!(
            tool.execute(json!({"command": "sleep 30 & echo $! > child.pid; wait"}))
                .await
                .unwrap_err()
                .contains("timed out")
        );
        let pid = std::fs::read_to_string(pid_file).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let status = std::process::Command::new("/bin/kill")
            .arg("-0")
            .arg(pid.trim())
            .status()
            .unwrap();
        assert!(!status.success(), "background child survived timeout");
    }
}
