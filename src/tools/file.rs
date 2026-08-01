use super::*;

use std::path::PathBuf;

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
            "Read a UTF-8-ish file. Relative paths are resolved inside the workspace and NEVER escape it (a capability boundary); external read-only roots — e.g. the main repository of a linked git worktree — are reachable ONLY via their absolute path. Lines are 1-indexed; long files are paged, use `offset` to continue reading.",
            json!({
                "path": {"type": "string", "description": "workspace-relative or authorized external absolute path"},
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
                "path": {"type": "string", "description": "workspace-relative or authorized external absolute path to an image file"}
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
                "path": {"type": "string", "description": "workspace-relative or authorized external absolute path"},
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
                "path": {"type": "string", "description": "workspace-relative or authorized external absolute path"},
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
        Ok(format!("file edited (line {line})"))
    }
}
