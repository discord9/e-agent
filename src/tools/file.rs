use super::*;

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

/// File tools only; the bash tool is added by the caller so it can be
/// bound to shared [`BackgroundTasks`] (subagents share task visibility and
/// cancellation, while their completions remain origin-session-local).
pub fn file_tools(workspace: &Workspace) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(ReadFile {
            workspace: workspace.clone(),
        }),
        Box::new(ReadImage::new(workspace.clone())),
        Box::new(WriteFile {
            workspace: workspace.clone(),
        }),
        Box::new(EditFile {
            workspace: workspace.clone(),
        }),
    ]
}

pub(super) struct ReadFile {
    pub(super) workspace: Workspace,
}

#[async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        spec(
            "read_file",
            "Read a UTF-8-ish file. Absolute paths pointing inside the workspace are accepted and treated as workspace-relative. Relative paths are resolved inside the workspace and NEVER escape it (a capability boundary); external read-only roots — e.g. the main repository of a linked git worktree — are reachable ONLY via their absolute path. Lines are 1-indexed; long files are paged, use `offset` to continue reading.",
            json!({
                "path": {"type": "string", "description": "workspace-relative or authorized external absolute path; absolute paths inside the workspace are also accepted (treated as workspace-relative)"},
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
        let path = required_string(&arguments, "path")?;
        // Image files are binary; point the model at read_image instead so
        // the image actually reaches the provider as an attachment.
        if crate::agent::image_mime_from_extension(path).is_some() {
            return Err(format!(
                "`{path}` looks like an image file; use `read_image` to attach it to the conversation"
            ));
        }
        let bytes = self.workspace.read(path)?;
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

/// Reads an image file (capability-path policy, same as read_file), stores
/// its bytes in the global content-addressed image store, and returns a
/// structured marker the runner turns into an attached image on the next
/// user message.
pub(super) struct ReadImage {
    pub(super) workspace: Workspace,
    pub(super) store: Option<PathBuf>,
}

impl ReadImage {
    fn new(workspace: Workspace) -> Self {
        Self {
            workspace,
            store: crate::agent::image_store_dir(),
        }
    }

    #[cfg(test)]
    pub(super) fn with_store(workspace: Workspace, store: PathBuf) -> Self {
        Self {
            workspace,
            store: Some(store),
        }
    }
}

#[async_trait]
impl Tool for ReadImage {
    fn spec(&self) -> ToolSpec {
        spec(
            "read_image",
            "Read an image file (png, jpeg/jpg, webp, gif; up to 10 MiB) and attach it to the conversation so the model can see it. Relative paths use the workspace; authorized external absolute paths are accepted. The image bytes are stored once in a global content-addressed cache and sent to the provider on the next model call.",
            json!({
                "path": {"type": "string", "description": "workspace-relative or authorized external absolute path to an image file; absolute paths inside the workspace are also accepted (treated as workspace-relative)"}
            }),
            &["path"],
        )
    }

    async fn execute(&self, arguments: Value) -> Result<String, String> {
        let path = required_string(&arguments, "path")?;
        let mime = crate::agent::image_mime_from_extension(path).ok_or_else(|| {
            format!("unsupported image type for {path}: expected .png, .jpeg/.jpg, .webp, or .gif")
        })?;
        let bytes = self.workspace.read(path)?;
        if bytes.len() > crate::agent::IMAGE_MAX_BYTES {
            return Err(format!(
                "image {path} is {} bytes, exceeding the {} MiB limit",
                bytes.len(),
                crate::agent::IMAGE_MAX_BYTES / (1024 * 1024)
            ));
        }
        let store = self
            .store
            .as_deref()
            .ok_or("no image store: XDG_STATE_HOME or HOME is not set")?;
        let hash = crate::agent::store_image_bytes(store, &bytes)?;
        Ok(format!(
            "{}{hash},{mime}{}[image read: {path}] (hash {hash}, {mime}, {} bytes)",
            crate::agent::IMAGE_MARKER_START,
            crate::agent::IMAGE_MARKER_END,
            bytes.len()
        ))
    }
}

pub(super) struct WriteFile {
    pub(super) workspace: Workspace,
}

#[async_trait]
impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        spec(
            "write_file",
            "Write a workspace-relative file or an authorized writable external absolute path, creating parent directories.",
            json!({
                "path": {"type": "string", "description": "workspace-relative or authorized writable external absolute path; absolute paths inside the workspace are also accepted (treated as workspace-relative)"},
                "content": {"type": "string", "description": "file contents"}
            }),
            &["path", "content"],
        )
    }

    async fn execute(&self, arguments: Value) -> Result<String, String> {
        let content = required_string(&arguments, "content")?;
        let path = required_string(&arguments, "path")?;
        // Snapshot before writing: the old content (or `None` when the file
        // did not exist) is what undo restores.
        let old_content = self.workspace.try_read_to_string(path)?;
        self.workspace.write(path, content)?;
        record_file_op(
            FileOpKind::Write,
            path,
            old_content,
            content.to_owned(),
            &self.workspace,
        );
        Ok("file written".into())
    }
}

/// Line-ending style of a file, detected from its content.
#[derive(Debug, Clone, Copy, PartialEq)]
enum LineEnding {
    /// CRLF (`\r\n`) — typical on Windows.
    Crlf,
    /// LF (`\n`) — typical on Unix.
    Lf,
}

/// Return (content with CRLF normalized to LF, detected line-ending style).
/// Mixed files are treated as CRLF if any CRLF is present (conservative:
/// preserves the dominant Windows style).
fn normalize_lf(content: &str) -> (String, LineEnding) {
    let has_crlf = content.contains("\r\n");
    let style = if has_crlf {
        LineEnding::Crlf
    } else {
        LineEnding::Lf
    };
    (content.replace("\r\n", "\n"), style)
}

/// Convert LF back to CRLF (used when writing to a CRLF-style file).
fn lf_to_crlf(content: &str) -> String {
    content.replace('\n', "\r\n")
}

pub(super) struct EditFile {
    pub(super) workspace: Workspace,
}

#[async_trait]
impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        spec(
            "edit_file",
            "Replace exactly one literal occurrence in a workspace-relative file or authorized writable external absolute path.",
            json!({
                "path": {"type": "string", "description": "workspace-relative or authorized writable external absolute path; absolute paths inside the workspace are also accepted (treated as workspace-relative)"},
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
        // Normalize line endings before matching: on Windows files are
        // typically CRLF, but the model's `old`/`new` usually use LF. Match
        // in LF space and restore the file's own line-ending style on write,
        // so an edit never flips a CRLF file to LF (or vice versa) and git
        // does not see fake whole-line diffs.
        let (content_lf, output_line_ending) = normalize_lf(&content);
        let old_lf = normalize_lf(old).0;
        let new_lf = normalize_lf(new).0;
        let count = content_lf.match_indices(&old_lf).count();
        if count != 1 {
            return Err(format!(
                "expected `old` exactly once, found {count} occurrences"
            ));
        }
        let start = content_lf.match_indices(&old_lf).next().unwrap().0;
        let line = content_lf[..start].matches('\n').count() + 1;
        let replaced = content_lf.replacen(&old_lf, &new_lf, 1);
        let output = match output_line_ending {
            LineEnding::Crlf => lf_to_crlf(&replaced),
            LineEnding::Lf => replaced,
        };
        self.workspace.write(path, output)?;
        // The edit's old/new fragments are the exact reverse parameters:
        // undo replaces `new` back with `old` (zero snapshot round-trip).
        record_file_op(
            FileOpKind::Edit,
            path,
            Some(old.to_owned()),
            new.to_owned(),
            &self.workspace,
        );
        Ok(format!("file edited (line {line})"))
    }
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Undo stack
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

/// A recorded file operation, kept in process memory so the user can undo
/// the most recent `edit_file` / `write_file`. Undo is a short-lived,
/// session-scoped convenience: the stack is never persisted.
#[derive(Debug)]
pub struct FileOpSnapshot {
    pub op: FileOpKind,
    /// The path as passed to the tool (workspace-relative or absolute
    /// external); undo applies to the same workspace the op ran on.
    pub path: String,
    /// `write`: the full old file content, `None` when the file did not
    /// exist before (undo = delete the file). `edit`: the `old` fragment.
    pub old_content: Option<String>,
    /// `write`: the full content that was written. `edit`: the `new`
    /// fragment that undo replaces back with `old_content`.
    pub new_content: String,
    /// Audit metadata required by the snapshot design (`{op, path,
    /// old_content, new_content, at, seq}`). Kept for debugging/audit; the
    /// current undo logic only consults the content fields.
    #[allow(dead_code)]
    pub at: std::time::Instant,
    #[allow(dead_code)]
    pub seq: u64,
    /// The workspace the operation was performed on.
    workspace: Workspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOpKind {
    Write,
    Edit,
}

/// Cap on the undo stack: undos are short-lived, so a bounded stack keeps
/// process memory from growing without bound (oldest entries are dropped).
pub(super) const UNDO_STACK_LIMIT: usize = 50;

pub(super) static UNDO_STACK: Mutex<Vec<FileOpSnapshot>> = Mutex::new(Vec::new());
static UNDO_SEQ: AtomicU64 = AtomicU64::new(0);

/// The undo stack, tolerant of poisoning: undo is a convenience feature and
/// must never panic the process over a poisoned lock.
fn undo_stack() -> std::sync::MutexGuard<'static, Vec<FileOpSnapshot>> {
    UNDO_STACK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Record a successful file operation for later undo. Called only after the
/// write/edit itself succeeded, so the stack only ever holds operations
/// that actually happened.
fn record_file_op(
    op: FileOpKind,
    path: &str,
    old_content: Option<String>,
    new_content: String,
    workspace: &Workspace,
) {
    let mut stack = undo_stack();
    stack.push(FileOpSnapshot {
        op,
        path: path.to_owned(),
        old_content,
        new_content,
        at: std::time::Instant::now(),
        seq: UNDO_SEQ.fetch_add(1, Ordering::Relaxed),
        workspace: workspace.clone(),
    });
    if stack.len() > UNDO_STACK_LIMIT {
        let overflow = stack.len() - UNDO_STACK_LIMIT;
        stack.drain(0..overflow);
    }
}

/// Undo the most recent file operation (`edit_file` / `write_file`) by
/// applying its exact reverse to the workspace the operation ran on.
///
/// On success the snapshot is popped — an undone operation cannot be undone
/// again. On failure the snapshot is restored so the user can retry once
/// the conflict is resolved. Failures are clear Chinese error messages,
/// never panics.
pub fn undo_file_op() -> Result<String, String> {
    let snapshot = undo_stack().pop();
    let Some(snapshot) = snapshot else {
        return Err("无法撤销：没有可撤销的文件操作".into());
    };
    let result = match snapshot.op {
        FileOpKind::Write => undo_write(
            &snapshot.workspace,
            &snapshot.path,
            snapshot.old_content.as_deref(),
            &snapshot.new_content,
        ),
        FileOpKind::Edit => undo_edit(
            &snapshot.workspace,
            &snapshot.path,
            snapshot.old_content.as_deref().unwrap_or_default(),
            &snapshot.new_content,
        ),
    };
    match result {
        Ok(message) => Ok(message),
        Err(error) => {
            undo_stack().push(snapshot);
            Err(error)
        }
    }
}

/// Reverse a `write`: the file must still contain exactly what was written
/// (otherwise a later modification would be clobbered); then restore the
/// old content, or delete the file when it did not exist before.
fn undo_write(
    workspace: &Workspace,
    path: &str,
    old_content: Option<&str>,
    new_content: &str,
) -> Result<String, String> {
    let current = workspace
        .try_read_to_string(path)
        .map_err(|error| format!("无法撤销 {path}：{error}"))?;
    let Some(current) = current else {
        return Err(format!("无法撤销 {path}：文件已不存在"));
    };
    if current != new_content {
        return Err(format!("无法撤销 {path}：文件已被后续修改"));
    }
    match old_content {
        Some(old) => {
            workspace
                .write(path, old)
                .map_err(|error| format!("无法撤销 {path}：{error}"))?;
            Ok(format!("已撤销 write_file: {path}"))
        }
        None => {
            workspace
                .remove_file(path)
                .map_err(|error| format!("无法撤销 {path}：{error}"))?;
            Ok(format!("已撤销 write_file: {path}（新建文件已删除）"))
        }
    }
}

/// Reverse an `edit`: the current file must still contain the `new` text
/// exactly once; replace it back with the `old` text, preserving the file's
/// line-ending style (mirror of [`EditFile::execute`]).
fn undo_edit(
    workspace: &Workspace,
    path: &str,
    old_fragment: &str,
    new_fragment: &str,
) -> Result<String, String> {
    let content = workspace
        .read_to_string(path)
        .map_err(|error| format!("无法撤销 {path}：{error}"))?;
    let (content_lf, output_line_ending) = normalize_lf(&content);
    let new_lf = normalize_lf(new_fragment).0;
    let old_lf = normalize_lf(old_fragment).0;
    if content_lf.match_indices(&new_lf).count() != 1 {
        return Err(format!("无法撤销 {path}：文件已被后续修改"));
    }
    let replaced = content_lf.replacen(&new_lf, &old_lf, 1);
    let output = match output_line_ending {
        LineEnding::Crlf => lf_to_crlf(&replaced),
        LineEnding::Lf => replaced,
    };
    workspace
        .write(path, output)
        .map_err(|error| format!("无法撤销 {path}：{error}"))?;
    Ok(format!("已撤销 edit_file: {path}"))
}

/// Empty the undo stack. Test-only: the stack is process-global, so tests
/// must clear it (under [`UNDO_TEST_LOCK`]) to isolate each scenario.
#[cfg(test)]
pub(super) fn clear_undo_stack() {
    undo_stack().clear();
}
