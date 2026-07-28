use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::agent::{AgentEvent, Tool, ToolSpec, preview};
use crate::workspace::Workspace;

const READ_LIMIT: usize = 64 * 1024;
const DEFAULT_READ_LINES: usize = 2000;
const OUTPUT_LIMIT: usize = 64 * 1024;

/// Built-in tools plus the shared background-task slots, exposed so that
/// other tools (e.g. delegate) can schedule background work too.
pub fn builtins(workspace: Workspace) -> (Vec<Box<dyn Tool>>, BackgroundTasks) {
    let background = BackgroundTasks::new(Duration::from_secs(30 * 60));
    let tools: Vec<Box<dyn Tool>> = vec![
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
            background: background.clone(),
        }),
    ];
    (tools, background)
}

struct ReadFile {
    workspace: Workspace,
}

#[async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        spec(
            "read_file",
            "Read a UTF-8-ish file from the workspace. Lines are 1-indexed; long files are paged, \
             use `offset` to continue reading.",
            json!({
                "path": {"type": "string", "description": "relative file path"},
                "offset": {"type": "integer", "description": "first line to read, 1-indexed (default 1)"},
                "limit": {"type": "integer", "description": format!("maximum lines to read (default {DEFAULT_READ_LINES})")}
            }),
            &["path"],
        )
    }

    async fn execute(&self, arguments: Value) -> Result<String, String> {
        let offset = optional_usize(&arguments, "offset")?.unwrap_or(1);
        if offset == 0 {
            return Err("`offset` must be >= 1".into());
        }
        let limit = optional_usize(&arguments, "limit")?.unwrap_or(DEFAULT_READ_LINES);
        if limit == 0 {
            return Err("`limit` must be >= 1".into());
        }
        let bytes = self.workspace.read(required_string(&arguments, "path")?)?;
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        let page: Vec<&str> = lines.iter().skip(offset - 1).take(limit).copied().collect();
        let mut output = page.join("\n");
        if output.len() > READ_LIMIT {
            output.truncate(READ_LIMIT);
            output.push_str("\n...[truncated]");
        }
        if offset > total && offset > 1 {
            output = format!("[offset {offset} is past end of file ({total} lines)]");
        } else {
            let end = offset - 1 + page.len();
            if end < total {
                output.push_str(&format!(
                    "\n[showing lines {offset}-{end} of {total}; use offset {} to continue]",
                    end + 1
                ));
            }
        }
        if output.is_empty() {
            output = "[empty file]".into();
        }
        Ok(output)
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
            &["path", "content"],
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
            &["path", "old", "new"],
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
        let count = content.match_indices(old).count();
        if count != 1 {
            return Err(format!(
                "expected `old` exactly once, found {count} occurrences"
            ));
        }
        let start = content.match_indices(old).next().unwrap().0;
        let line = content[..start].matches('\n').count() + 1;
        self.workspace.write(path, content.replacen(old, new, 1))?;
        Ok(format!("file edited (line {line})"))
    }
}

struct Bash {
    workspace: Workspace,
    timeout: Duration,
    background: BackgroundTasks,
}

#[async_trait]
impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "Run a shell command with the workspace as its current directory.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "shell command"},
                    "background": {"type": "boolean", "description": "run without blocking; completion is delivered as an event and injected into the next model turn"}
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<String, String> {
        let command = required_string(&arguments, "command")?;
        if optional_bool(&arguments, "background")? {
            return self
                .background
                .start(self.workspace.clone(), command.to_owned());
        }
        run_bash(&self.workspace, command, self.timeout, None).await
    }

    fn set_event_sender(&mut self, sender: tokio::sync::mpsc::UnboundedSender<AgentEvent>) {
        self.background.sender = Some(sender);
    }
}

/// Maximum number of concurrently running background tasks.
pub const MAX_BACKGROUND: usize = 4;

#[derive(Clone)]
pub struct BackgroundTasks {
    next_id: Arc<AtomicU64>,
    slots: Arc<std::sync::Mutex<Vec<Option<BackgroundSlot>>>>,
    sender: Option<tokio::sync::mpsc::UnboundedSender<AgentEvent>>,
    timeout: Duration,
}

#[derive(Clone)]
struct BackgroundSlot {
    id: u64,
    process_group: Arc<AtomicI32>,
}

impl BackgroundTasks {
    fn new(timeout: Duration) -> Self {
        Self {
            next_id: Arc::new(AtomicU64::new(1)),
            slots: Arc::new(std::sync::Mutex::new(vec![None; MAX_BACKGROUND])),
            sender: None,
            timeout,
        }
    }

    /// Start a background bash command. Returns a human-readable "started"
    /// message containing the task id, or an error if all slots are in use.
    pub fn start(&self, workspace: Workspace, command: String) -> Result<String, String> {
        let process_group = Arc::new(AtomicI32::new(0));
        let pg = process_group.clone();
        let timeout = self.timeout;
        self.spawn(preview(&command, 100), Some(process_group), move || {
            let workspace = workspace.clone();
            async move {
                match run_bash(&workspace, &command, timeout, Some(pg)).await {
                    Ok(output) | Err(output) => output,
                }
            }
        })
    }

    /// Allocate a slot and spawn a background future. Completion is delivered
    /// as [`AgentEvent::BackgroundCompleted`]. `process_group` is only used
    /// for kill-on-drop cleanup; pass `None` for non-process tasks.
    pub fn spawn<F, Fut>(
        &self,
        label: String,
        process_group: Option<Arc<AtomicI32>>,
        work: F,
    ) -> Result<String, String>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = String> + Send + 'static,
    {
        let sender = self
            .sender
            .clone()
            .ok_or("background task delivery is unavailable")?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let slot = BackgroundSlot {
            id,
            process_group: process_group.unwrap_or_else(|| Arc::new(AtomicI32::new(0))),
        };
        {
            let mut slots = self.slots.lock().unwrap();
            let empty = slots.iter_mut().find(|slot| slot.is_none());
            let Some(empty) = empty else {
                return Err(format!(
                    "all {MAX_BACKGROUND} background task slots are in use"
                ));
            };
            *empty = Some(slot);
        }
        let started = format!("started background task {id}: {label}");
        let slots = self.slots.clone();
        tokio::spawn(async move {
            let output = work().await;
            slots
                .lock()
                .unwrap()
                .retain(|slot| slot.as_ref().map(|slot| slot.id != id).unwrap_or(true));
            let _ = sender.send(AgentEvent::BackgroundCompleted { id, output });
        });
        Ok(started)
    }
}

impl Drop for BackgroundTasks {
    fn drop(&mut self) {
        #[cfg(unix)]
        for slot in self.slots.lock().unwrap().iter().flatten() {
            if let Some(process_group) =
                rustix::process::Pid::from_raw(slot.process_group.load(Ordering::Acquire))
            {
                let _ = rustix::process::kill_process_group(
                    process_group,
                    rustix::process::Signal::KILL,
                );
            }
        }
    }
}

#[cfg(unix)]
struct ProcessGroupGuard {
    process_group: Option<rustix::process::Pid>,
}

#[cfg(unix)]
impl ProcessGroupGuard {
    fn armed(process_group: rustix::process::Pid) -> Self {
        Self {
            process_group: Some(process_group),
        }
    }

    fn disarm(&mut self) {
        self.process_group = None;
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if let Some(process_group) = self.process_group {
            let _ =
                rustix::process::kill_process_group(process_group, rustix::process::Signal::KILL);
        }
    }
}

async fn run_bash(
    workspace: &Workspace,
    command: &str,
    timeout: Duration,
    process_group_slot: Option<Arc<AtomicI32>>,
) -> Result<String, String> {
    let mut process = Command::new("/bin/bash");
    process
        .arg("-lc")
        .arg(command)
        .current_dir(workspace.root())
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
    #[cfg(unix)]
    if let Some(slot) = &process_group_slot {
        slot.store(process_group.as_raw_nonzero().get(), Ordering::Release);
    }
    // Kills the process group if this future is dropped mid-execution
    // (e.g. the user cancelled the turn).
    #[cfg(unix)]
    let mut cancel_guard = ProcessGroupGuard::armed(process_group);
    let stdout = child.stdout.take().ok_or("failed to capture stdout")?;
    let stderr = child.stderr.take().ok_or("failed to capture stderr")?;
    let result = tokio::time::timeout(timeout, async {
        let (stdout, stderr, status) = tokio::join!(capture(stdout), capture(stderr), child.wait());
        Ok::<_, std::io::Error>((stdout?, stderr?, status?))
    })
    .await;
    let (stdout, stderr, status) = match result {
        Ok(result) => result.map_err(|error| format!("shell I/O failed: {error}"))?,
        Err(_) => {
            #[cfg(unix)]
            let kill_error = match rustix::process::kill_process_group(
                process_group,
                rustix::process::Signal::KILL,
            ) {
                Ok(()) | Err(rustix::io::Errno::SRCH) => None,
                Err(error) => Some(error),
            };
            #[cfg(unix)]
            let _ = child.wait().await;
            #[cfg(not(unix))]
            let _ = child.kill().await;
            #[cfg(not(unix))]
            let _ = child.wait().await;
            if let Some(slot) = &process_group_slot {
                slot.store(0, Ordering::Release);
            }
            #[cfg(unix)]
            if let Some(error) = kill_error {
                return Err(format!("failed to kill bash process group: {error}"));
            }
            return Err(format!(
                "exit code: signal\nstdout:\n\nstderr:\n\n[command timed out after {} seconds]",
                timeout.as_secs_f64()
            ));
        }
    };
    #[cfg(unix)]
    cancel_guard.disarm();
    if let Some(slot) = &process_group_slot {
        slot.store(0, Ordering::Release);
    }
    let text = format_output(status.code(), &stdout, &stderr);
    if status.success() {
        Ok(text)
    } else {
        Err(text)
    }
}

fn spec(name: &str, description: &str, properties: Value, required: &[&str]) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        parameters: json!({"type": "object", "properties": properties, "required": required}),
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

fn optional_usize(arguments: &Value, name: &str) -> Result<Option<usize>, String> {
    arguments
        .as_object()
        .ok_or("tool arguments must be a JSON object")?
        .get(name)
        .map(|value| {
            value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| format!("`{name}` must be a non-negative integer"))
        })
        .transpose()
}

fn optional_bool(arguments: &Value, name: &str) -> Result<bool, String> {
    arguments
        .as_object()
        .ok_or("tool arguments must be a JSON object")?
        .get(name)
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| format!("`{name}` must be a boolean"))
        })
        .transpose()
        .map(|value| value.unwrap_or(false))
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
        assert_eq!(
            edit(&temp, "two", "x").await.unwrap(),
            "file edited (line 1)"
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "one x one");
    }

    async fn read(temp: &tempfile::TempDir, arguments: Value) -> Result<String, String> {
        ReadFile {
            workspace: Workspace::new(temp.path()).unwrap(),
        }
        .execute(arguments)
        .await
    }

    #[tokio::test]
    async fn read_pages_lines_with_a_continuation_hint() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("file.txt"), "a\nb\nc\nd\ne\n").unwrap();
        assert_eq!(
            read(&temp, json!({"path": "file.txt", "limit": 2}))
                .await
                .unwrap(),
            "a\nb\n[showing lines 1-2 of 5; use offset 3 to continue]"
        );
        assert_eq!(
            read(&temp, json!({"path": "file.txt", "offset": 3, "limit": 2}))
                .await
                .unwrap(),
            "c\nd\n[showing lines 3-4 of 5; use offset 5 to continue]"
        );
        assert_eq!(
            read(&temp, json!({"path": "file.txt", "offset": 5}))
                .await
                .unwrap(),
            "e"
        );
    }

    #[tokio::test]
    async fn read_reports_offset_past_end_and_empty_files() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("file.txt"), "a\nb\n").unwrap();
        assert_eq!(
            read(&temp, json!({"path": "file.txt", "offset": 9}))
                .await
                .unwrap(),
            "[offset 9 is past end of file (2 lines)]"
        );
        std::fs::write(temp.path().join("empty.txt"), "").unwrap();
        assert_eq!(
            read(&temp, json!({"path": "empty.txt"})).await.unwrap(),
            "[empty file]"
        );
    }

    #[tokio::test]
    async fn read_truncates_long_lines_to_the_byte_limit() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("file.txt"), "x".repeat(READ_LIMIT + 1)).unwrap();
        let output = read(&temp, json!({"path": "file.txt"})).await.unwrap();
        assert!(output.ends_with("\n...[truncated]"));
        assert_eq!(output.len(), READ_LIMIT + "\n...[truncated]".len());
    }

    #[tokio::test]
    async fn read_rejects_invalid_paging_arguments() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("file.txt"), "a\n").unwrap();
        assert!(
            read(&temp, json!({"path": "file.txt", "offset": 0}))
                .await
                .unwrap_err()
                .contains(">= 1")
        );
        assert!(
            read(&temp, json!({"path": "file.txt", "limit": 0}))
                .await
                .unwrap_err()
                .contains(">= 1")
        );
        assert!(
            read(&temp, json!({"path": "file.txt", "offset": "x"}))
                .await
                .unwrap_err()
                .contains("non-negative integer")
        );
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
            background: BackgroundTasks::new(Duration::from_secs(30 * 60)),
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

    fn background_bash(
        temp: &tempfile::TempDir,
        timeout: Duration,
    ) -> (Bash, tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut bash = Bash {
            workspace: Workspace::new(temp.path()).unwrap(),
            timeout: Duration::from_secs(30),
            background: BackgroundTasks::new(timeout),
        };
        bash.set_event_sender(sender);
        (bash, receiver)
    }

    #[tokio::test]
    async fn allows_up_to_max_background_tasks() {
        let temp = tempfile::tempdir().unwrap();
        let (bash, _) = background_bash(&temp, Duration::from_secs(10));
        for id in 1..=MAX_BACKGROUND {
            assert!(
                bash.execute(json!({"command": "sleep 10", "background": true}))
                    .await
                    .unwrap()
                    .starts_with(&format!("started background task {id}:"))
            );
        }
        assert!(
            bash.execute(json!({"command": "true", "background": true}))
                .await
                .unwrap_err()
                .contains("background task slots are in use")
        );
    }

    #[tokio::test]
    async fn background_timeout_is_delivered_as_completion() {
        let temp = tempfile::tempdir().unwrap();
        let (bash, mut receiver) = background_bash(&temp, Duration::from_millis(50));
        bash.execute(
            json!({"command": "sleep 30 & echo $! > child.pid; wait", "background": true}),
        )
        .await
        .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            event,
            AgentEvent::BackgroundCompleted { output, .. } if output.contains("timed out")
        ));
        let pid = std::fs::read_to_string(temp.path().join("child.pid")).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !std::process::Command::new("/bin/kill")
                .arg("-0")
                .arg(pid.trim())
                .status()
                .unwrap()
                .success()
        );
    }
}
