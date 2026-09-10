use std::path::{Component, Path, PathBuf};
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;
use cap_std::fs::Dir;
use serde::Serialize;
use serde_json::{Value, json};

use crate::agent::{Tool, ToolOutput, ToolSpec};
use crate::workspace::Workspace;

const DEFAULT_LIMIT: usize = 100;
const READ_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorKind {
    List,
    Search,
}

#[derive(Debug)]
struct Cursor {
    token: String,
    kind: CursorKind,
    items: Vec<String>,
    errors: Vec<NoteError>,
    position: usize,
}

#[derive(Debug, Clone, Serialize)]
struct NoteError {
    path: String,
    reason: String,
}

struct Files {
    paths: Vec<String>,
    errors: Vec<NoteError>,
}

pub struct Notes {
    workspace: Workspace,
    read_only: bool,
    cursor: Mutex<Option<Cursor>>,
    next_token: AtomicU64,
}

pub fn notes_tool(workspace: &Workspace, read_only: bool) -> Box<dyn Tool> {
    Box::new(Notes::new(workspace.clone(), read_only))
}

impl Notes {
    pub fn new(workspace: Workspace, read_only: bool) -> Self {
        Self {
            workspace,
            read_only,
            cursor: Mutex::new(None),
            next_token: AtomicU64::new(1),
        }
    }

    fn path(value: Option<&Value>, required: bool) -> Result<PathBuf, String> {
        let text = value.and_then(Value::as_str);
        if required && text.is_none() {
            return Err("notes `path` must be a non-empty string".into());
        }
        let text = text.unwrap_or("");
        if text.is_empty() {
            return if required {
                Err("notes `path` must be a non-empty string".into())
            } else {
                Ok(PathBuf::new())
            };
        }
        let path = Path::new(text);
        if path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err("notes path must be relative and contain only normal components".into());
        }
        Ok(path.to_owned())
    }

    fn fields(object: &serde_json::Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
        if let Some(name) = object.keys().find(|name| !allowed.contains(&name.as_str())) {
            return Err(format!(
                "notes field `{name}` is not allowed for this action"
            ));
        }
        Ok(())
    }

    fn limit(value: Option<&Value>, default: usize) -> Result<usize, String> {
        let limit = value.map_or(Ok(default), |value| {
            value
                .as_u64()
                .and_then(|number| usize::try_from(number).ok())
                .ok_or_else(|| "notes `limit` must be a positive integer".to_owned())
        })?;
        if limit == 0 {
            return Err("notes `limit` must be a positive integer".into());
        }
        Ok(limit)
    }

    fn cursor_arg(object: &serde_json::Map<String, Value>) -> Result<Option<&str>, String> {
        match object.get("cursor") {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_str()
                .map(Some)
                .ok_or("notes `cursor` must be a string or null".into()),
        }
    }

    fn next_token(&self) -> String {
        format!("notes-{}", self.next_token.fetch_add(1, Ordering::Relaxed))
    }

    fn path_label(path: &Path) -> String {
        if path.as_os_str().is_empty() {
            ".".into()
        } else {
            path.to_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{path:?}"))
        }
    }

    fn error(errors: &mut Vec<NoteError>, path: &Path, reason: impl Into<String>) {
        errors.push(NoteError {
            path: Self::path_label(path),
            reason: reason.into(),
        });
    }

    fn list_files(
        directory: &Dir,
        prefix: &Path,
        output: &mut Vec<String>,
        errors: &mut Vec<NoteError>,
    ) {
        let entries = match directory.entries() {
            Ok(entries) => entries,
            Err(error) => {
                Self::error(errors, prefix, format!("list failed: {error}"));
                return;
            }
        };
        let mut entries = entries
            .filter_map(|entry| match entry {
                Ok(entry) => Some(entry),
                Err(error) => {
                    Self::error(errors, prefix, format!("directory entry failed: {error}"));
                    None
                }
            })
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            let path = prefix.join(&name);
            let kind = match entry.file_type() {
                Ok(kind) => kind,
                Err(error) => {
                    Self::error(errors, &path, format!("metadata failed: {error}"));
                    continue;
                }
            };
            if kind.is_symlink() {
                continue;
            }
            let Some(name) = name.to_str() else {
                Self::error(errors, &path, "path is not valid UTF-8");
                continue;
            };
            let path = prefix.join(name);
            if kind.is_dir() {
                match entry.open_dir() {
                    Ok(child) => Self::list_files(&child, &path, output, errors),
                    Err(error) => {
                        Self::error(errors, &path, format!("open directory failed: {error}"));
                    }
                }
            } else if kind.is_file() {
                output.push(path.to_str().expect("listed paths are UTF-8").to_owned());
            }
        }
    }

    fn files(&self, scope: &Path) -> Result<Option<Files>, String> {
        let Some(root) = self
            .workspace
            .try_open_dir(".e-agent/notes")
            .map_err(|error| format!("notes root failed: {error}"))?
        else {
            return Ok(None);
        };
        let directory = if scope.as_os_str().is_empty() {
            root
        } else {
            root.open_dir(scope).map_err(|error| {
                format!(
                    "notes directory `{}` failed: {error}",
                    Self::path_label(scope)
                )
            })?
        };
        let mut paths = Vec::new();
        let mut errors = Vec::new();
        Self::list_files(&directory, scope, &mut paths, &mut errors);
        paths.sort();
        Ok(Some(Files { paths, errors }))
    }

    fn page_locked(
        slot: &mut Option<Cursor>,
        kind: CursorKind,
        limit: usize,
    ) -> Result<ToolOutput, String> {
        let cursor = slot.as_mut().ok_or("notes cursor is missing or expired")?;
        if cursor.kind != kind {
            return Err(if kind == CursorKind::List {
                "notes cursor belongs to a search; start a new list"
            } else {
                "notes cursor belongs to a list; start a new search"
            }
            .into());
        }
        let end = cursor
            .position
            .saturating_add(limit)
            .min(cursor.items.len());
        let page = cursor.items[cursor.position..end].to_vec();
        let errors = cursor.errors.clone();
        let incomplete = !errors.is_empty();
        let has_more = end < cursor.items.len();
        cursor.position = end;
        let token = cursor.token.clone();
        if !has_more {
            *slot = None;
        }
        let key = if kind == CursorKind::List {
            "notes"
        } else {
            "results"
        };
        Ok(ToolOutput::text(
            json!({
                key: page,
                "has_more": has_more,
                "cursor": has_more.then_some(token),
                "incomplete": incomplete,
                "errors": errors,
            })
            .to_string(),
        ))
    }

    fn start_page(
        &self,
        kind: CursorKind,
        items: Vec<String>,
        errors: Vec<NoteError>,
        limit: usize,
    ) -> Result<ToolOutput, String> {
        let mut slot = self
            .cursor
            .lock()
            .map_err(|_| "notes cursor lock is poisoned".to_owned())?;
        *slot = Some(Cursor {
            token: self.next_token(),
            kind,
            items,
            errors,
            position: 0,
        });
        Self::page_locked(&mut slot, kind, limit)
    }

    fn continue_page(
        &self,
        kind: CursorKind,
        token: &str,
        limit: usize,
    ) -> Result<ToolOutput, String> {
        let mut slot = self
            .cursor
            .lock()
            .map_err(|_| "notes cursor lock is poisoned".to_owned())?;
        let current = slot.as_ref().ok_or("notes cursor is missing or expired")?;
        if current.token != token {
            return Err("notes cursor is missing or expired".into());
        }
        Self::page_locked(&mut slot, kind, limit)
    }

    fn list(&self, object: &serde_json::Map<String, Value>) -> Result<ToolOutput, String> {
        let cursor = Self::cursor_arg(object)?;
        let limit = Self::limit(object.get("limit"), DEFAULT_LIMIT)?;
        if let Some(token) = cursor {
            Self::fields(object, &["action", "cursor", "limit"])?;
            return self.continue_page(CursorKind::List, token, limit);
        }
        Self::fields(object, &["action", "path", "cursor", "limit"])?;
        let scope = Self::path(object.get("path"), false)?;
        let Some(items) = self.files(&scope)? else {
            *self
                .cursor
                .lock()
                .map_err(|_| "notes cursor lock is poisoned".to_owned())? = None;
            return Ok(ToolOutput::text(
                json!({"notes": [], "has_more": false, "cursor": null, "incomplete": false, "errors": []}).to_string(),
            ));
        };
        self.start_page(CursorKind::List, items.paths, items.errors, limit)
    }

    fn note_text(&self, path: &Path) -> Result<String, String> {
        let label = Self::path_label(path);
        let logical = Path::new(".e-agent/notes").join(path);
        let bytes = self
            .workspace
            .read(&logical.to_string_lossy())
            .map_err(|error| format!("note `{label}` read failed: {error}"))?;
        if bytes.len() > READ_LIMIT {
            return Err(format!("note `{label}` exceeds the 64 KiB read limit"));
        }
        String::from_utf8(bytes).map_err(|_| format!("note `{label}` is not valid UTF-8"))
    }

    fn search(&self, object: &serde_json::Map<String, Value>) -> Result<ToolOutput, String> {
        let cursor = Self::cursor_arg(object)?;
        let limit = Self::limit(object.get("limit"), DEFAULT_LIMIT)?;
        if let Some(token) = cursor {
            Self::fields(object, &["action", "cursor", "limit"])?;
            return self.continue_page(CursorKind::Search, token, limit);
        }
        Self::fields(object, &["action", "path", "cursor", "limit", "query"])?;
        let scope = Self::path(object.get("path"), false)?;
        let query = object
            .get("query")
            .and_then(Value::as_str)
            .ok_or("notes `query` must be a string")?
            .to_lowercase();
        let Some(files) = self.files(&scope)? else {
            *self
                .cursor
                .lock()
                .map_err(|_| "notes cursor lock is poisoned".to_owned())? = None;
            return Ok(ToolOutput::text(
                json!({"results": [], "has_more": false, "cursor": null, "incomplete": false, "errors": []}).to_string(),
            ));
        };
        let mut results = Vec::new();
        let mut errors = files.errors;
        for path in files.paths {
            match self.note_text(Path::new(&path)) {
                Ok(text) => {
                    for (line, content) in text.lines().enumerate() {
                        if content.to_lowercase().contains(&query) {
                            results.push(format!("{path}:{}: {content}", line + 1));
                        }
                    }
                }
                Err(reason) => Self::error(&mut errors, Path::new(&path), reason),
            }
        }
        self.start_page(CursorKind::Search, results, errors, limit)
    }

    fn read(&self, object: &serde_json::Map<String, Value>) -> Result<ToolOutput, String> {
        let path = Self::path(object.get("path"), true)?;
        let offset = super::optional_usize(&Value::Object(object.clone()), "offset")?.unwrap_or(0);
        let limit = Self::limit(object.get("limit"), READ_LIMIT)?;
        if limit > READ_LIMIT {
            return Err(format!(
                "notes `limit` must not exceed {READ_LIMIT} for note `{}`",
                Self::path_label(&path)
            ));
        }
        let text = self.note_text(&path)?;
        if offset > text.len() {
            return Err(format!(
                "notes `offset` is past the end of note `{}`",
                Self::path_label(&path)
            ));
        }
        if !text.is_char_boundary(offset) {
            return Err(format!(
                "notes `offset` must be a UTF-8 byte boundary in note `{}`",
                Self::path_label(&path)
            ));
        }
        let mut end = offset.saturating_add(limit).min(text.len());
        while end > offset && !text.is_char_boundary(end) {
            end -= 1;
        }
        let has_more = end < text.len();
        Ok(ToolOutput::text(
            json!({
                "path": path.to_string_lossy(), "content": &text[offset..end], "offset": offset,
                "next_offset": has_more.then_some(end), "has_more": has_more
            })
            .to_string(),
        ))
    }

    fn write(&self, object: &serde_json::Map<String, Value>) -> Result<ToolOutput, String> {
        let path = Self::path(object.get("path"), true)?;
        let content = object
            .get("content")
            .and_then(Value::as_str)
            .ok_or("notes `content` must be a string")?;
        let logical = Path::new(".e-agent/notes").join(&path);
        self.workspace
            .write(&logical.to_string_lossy(), content.as_bytes())?;
        Ok(ToolOutput::text(
            json!({"written": path.to_string_lossy()}).to_string(),
        ))
    }
}

#[async_trait]
impl Tool for Notes {
    fn spec(&self) -> ToolSpec {
        let mut properties = json!({
            "action": {"type":"string", "enum": if self.read_only {json!(["list","read","search"])} else {json!(["list","read","search","write"]) }},
            "path": {"type":"string", "description":"path relative to .e-agent/notes"},
            "cursor": {"type":["string","null"], "description":"process-local continuation cursor"},
            "limit": {"type":"integer", "minimum":1},
            "query": {"type":"string"},
            "offset": {"type":"integer", "minimum":0}
        });
        if !self.read_only {
            properties["content"] = json!({"type":"string"});
        }
        ToolSpec {
            name: "notes".into(),
            description: "Project-shared UTF-8 notes rooted at .e-agent/notes. List and search return deterministic pages with one process-local cursor per tool instance; a new list or search replaces it, and cursors are not persisted. Continuations accept only action, cursor, and limit. Read uses stateless UTF-8 byte paging. Write replaces a complete note and is unavailable in read-only roles.".into(),
            parameters: json!({"type":"object", "properties":properties, "required":["action"]}),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, String> {
        let object = arguments
            .as_object()
            .ok_or("notes arguments must be a JSON object")?;
        let action = object
            .get("action")
            .and_then(Value::as_str)
            .ok_or("notes `action` must be a string")?;
        match action {
            "list" => self.list(object),
            "search" => self.search(object),
            "read" => {
                Self::fields(object, &["action", "path", "offset", "limit"])?;
                self.read(object)
            }
            "write" if !self.read_only => {
                Self::fields(object, &["action", "path", "content"])?;
                self.write(object)
            }
            "write" => Err("notes write is unavailable to read-only roles".into()),
            _ => Err("notes action must be one of: list, read, search, write".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tool(temp: &tempfile::TempDir, read_only: bool) -> Notes {
        Notes::new(Workspace::new(temp.path()).unwrap(), read_only)
    }
    fn output(value: ToolOutput) -> Value {
        serde_json::from_str(&value.content).unwrap()
    }

    #[tokio::test]
    async fn notes_list_cursor_continuation_and_deterministic_completion() {
        let temp = tempfile::tempdir().unwrap();
        let notes = tool(&temp, false);
        for path in ["a", "b", "c"] {
            notes
                .execute(json!({"action":"write","path":path,"content":path}))
                .await
                .unwrap();
        }
        let first = output(
            notes
                .execute(json!({"action":"list","limit":1}))
                .await
                .unwrap(),
        );
        let second = output(
            notes
                .execute(json!({"action":"list","cursor":first["cursor"],"limit":1}))
                .await
                .unwrap(),
        );
        let third = output(
            notes
                .execute(json!({"action":"list","cursor":second["cursor"],"limit":1}))
                .await
                .unwrap(),
        );
        assert_eq!(first["notes"], json!(["a"]));
        assert_eq!(second["notes"], json!(["b"]));
        assert_eq!(third["notes"], json!(["c"]));
        assert!(!third["has_more"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn notes_search_cursor_replacement_and_wrong_kind_token() {
        let temp = tempfile::tempdir().unwrap();
        let notes = tool(&temp, false);
        for (path, content) in [("a", "hit"), ("b", "hit"), ("c", "hit")] {
            notes
                .execute(json!({"action":"write","path":path,"content":content}))
                .await
                .unwrap();
        }
        let search = output(
            notes
                .execute(json!({"action":"search","query":"hit","limit":1}))
                .await
                .unwrap(),
        );
        let list = output(
            notes
                .execute(json!({"action":"list","limit":1}))
                .await
                .unwrap(),
        );
        assert!(
            notes
                .execute(json!({"action":"search","cursor":search["cursor"],"limit":1}))
                .await
                .is_err()
        );
        let next = output(
            notes
                .execute(json!({"action":"list","cursor":list["cursor"],"limit":1}))
                .await
                .unwrap(),
        );
        assert_eq!(next["notes"], json!(["b"]));
    }

    #[tokio::test]
    async fn notes_stale_same_kind_cursor_cannot_consume_or_overwrite_newer_cursor() {
        let temp = tempfile::tempdir().unwrap();
        let notes = tool(&temp, false);
        for path in ["a", "b", "c"] {
            notes
                .execute(json!({"action":"write","path":path,"content":path}))
                .await
                .unwrap();
        }
        let a = output(
            notes
                .execute(json!({"action":"list","limit":1}))
                .await
                .unwrap(),
        );
        let b = output(
            notes
                .execute(json!({"action":"list","limit":1}))
                .await
                .unwrap(),
        );
        assert!(
            notes
                .execute(json!({"action":"list","cursor":a["cursor"],"limit":1}))
                .await
                .is_err()
        );
        let next = output(
            notes
                .execute(json!({"action":"list","cursor":b["cursor"],"limit":1}))
                .await
                .unwrap(),
        );
        assert_eq!(next["notes"], json!(["b"]));
    }

    #[tokio::test]
    async fn notes_no_cursor_or_error_and_malformed_args() {
        let temp = tempfile::tempdir().unwrap();
        let notes = tool(&temp, false);
        assert!(
            notes
                .execute(json!({"action":"list","cursor":"missing"}))
                .await
                .is_err()
        );
        assert!(
            notes
                .execute(json!({"action":"search","cursor":"missing"}))
                .await
                .is_err()
        );
        assert!(notes.execute(json!({"action":"wat"})).await.is_err());
        assert!(
            notes
                .execute(json!({"action":"list","cursor":"x","path":"scope"}))
                .await
                .is_err()
        );
        assert!(
            notes
                .execute(json!({"action":"read","path":"x","extra":1}))
                .await
                .is_err()
        );
        assert!(
            notes
                .execute(json!({"action":"write","path":"x","content":1}))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn notes_read_utf8_byte_boundaries_and_write_read_behavior() {
        let temp = tempfile::tempdir().unwrap();
        let notes = tool(&temp, false);
        notes
            .execute(json!({"action":"write","path":"nested/utf8","content":"a你b好c"}))
            .await
            .unwrap();
        let first = output(
            notes
                .execute(json!({"action":"read","path":"nested/utf8","offset":0,"limit":4}))
                .await
                .unwrap(),
        );
        assert_eq!(first["content"], "a你");
        assert_eq!(first["next_offset"], 4);
        let second = output(
            notes
                .execute(json!({"action":"read","path":"nested/utf8","offset":4,"limit":4}))
                .await
                .unwrap(),
        );
        assert_eq!(second["content"], "b好");
        assert!(
            notes
                .execute(json!({"action":"read","path":"nested/utf8","offset":2,"limit":2}))
                .await
                .is_err()
        );
        notes
            .execute(json!({"action":"write","path":"nested/utf8","content":"replaced"}))
            .await
            .unwrap();
        assert_eq!(
            output(
                notes
                    .execute(json!({"action":"read","path":"nested/utf8"}))
                    .await
                    .unwrap()
            )["content"],
            "replaced"
        );
    }

    #[tokio::test]
    async fn notes_path_traversal_and_symlink_authorization() {
        let temp = tempfile::tempdir().unwrap();
        let notes = tool(&temp, false);
        for path in ["../escape", "a/../../escape", "/tmp/escape", "./escape"] {
            assert!(
                notes
                    .execute(json!({"action":"read","path":path}))
                    .await
                    .is_err()
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let root = temp.path().join(".e-agent/notes");
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("target"), "inside").unwrap();
            symlink("target", root.join("internal-link")).unwrap();
            let outside = tempfile::tempdir().unwrap();
            fs::write(outside.path().join("secret"), "secret").unwrap();
            symlink(outside.path(), root.join("escape")).unwrap();
            assert_eq!(
                notes
                    .workspace
                    .read(".e-agent/notes/internal-link")
                    .unwrap(),
                b"inside"
            );
            let listed = output(notes.execute(json!({"action":"list"})).await.unwrap());
            assert_eq!(listed["notes"], json!(["target"]));
            assert!(
                notes
                    .execute(json!({"action":"read","path":"escape/secret"}))
                    .await
                    .is_err()
            );
            assert!(
                notes
                    .execute(json!({"action":"write","path":"escape/new","content":"x"}))
                    .await
                    .is_err()
            );
            assert!(!outside.path().join("new").exists());
        }
    }

    #[tokio::test]
    async fn notes_list_does_not_decode_note_content() {
        let temp = tempfile::tempdir().unwrap();
        let notes = tool(&temp, false);
        let root = temp.path().join(".e-agent/notes");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("binary"), [0xff]).unwrap();
        fs::write(root.join("healthy"), "text").unwrap();

        let listed = output(notes.execute(json!({"action":"list"})).await.unwrap());
        assert_eq!(listed["notes"], json!(["binary", "healthy"]));
        assert_eq!(listed["incomplete"], false);
        assert_eq!(listed["errors"], json!([]));
    }

    #[tokio::test]
    async fn notes_list_specified_scope_failure_is_not_empty_success() {
        let temp = tempfile::tempdir().unwrap();
        let notes = tool(&temp, false);
        let root = temp.path().join(".e-agent/notes");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("file"), "not a directory").unwrap();

        let error = notes
            .execute(json!({"action":"list","path":"file"}))
            .await
            .unwrap_err();
        assert!(error.contains("notes directory `file` failed"));
    }

    #[tokio::test]
    async fn notes_search_keeps_healthy_results_and_reports_bad_notes() {
        let temp = tempfile::tempdir().unwrap();
        let notes = tool(&temp, false);
        let root = temp.path().join(".e-agent/notes");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a"), "match first").unwrap();
        fs::write(root.join("b"), [0xff]).unwrap();
        fs::write(root.join("c"), "match last").unwrap();

        let result = output(
            notes
                .execute(json!({"action":"search","query":"match"}))
                .await
                .unwrap(),
        );
        assert_eq!(
            result["results"],
            json!(["a:1: match first", "c:1: match last"])
        );
        assert_eq!(result["incomplete"], true);
        assert_eq!(
            result["errors"],
            json!([{
                "path": "b",
                "reason": "note `b` is not valid UTF-8"
            }])
        );
    }

    #[tokio::test]
    async fn notes_search_distinguishes_invalid_notes_from_no_matches_and_oversize() {
        let temp = tempfile::tempdir().unwrap();
        let notes = tool(&temp, false);
        let root = temp.path().join(".e-agent/notes");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("invalid"), [0xff]).unwrap();
        fs::write(root.join("large"), vec![b'x'; READ_LIMIT + 1]).unwrap();

        let incomplete = output(
            notes
                .execute(json!({"action":"search","query":"missing"}))
                .await
                .unwrap(),
        );
        assert_eq!(incomplete["results"], json!([]));
        assert_eq!(incomplete["incomplete"], true);
        assert_eq!(incomplete["errors"].as_array().unwrap().len(), 2);
        assert_eq!(incomplete["errors"][0]["path"], "invalid");
        assert!(
            incomplete["errors"][1]["reason"]
                .as_str()
                .unwrap()
                .contains("64 KiB")
        );

        fs::remove_file(root.join("invalid")).unwrap();
        fs::remove_file(root.join("large")).unwrap();
        fs::write(root.join("plain"), "unrelated").unwrap();
        let complete = output(
            notes
                .execute(json!({"action":"search","query":"missing"}))
                .await
                .unwrap(),
        );
        assert_eq!(complete["results"], json!([]));
        assert_eq!(complete["incomplete"], false);
        assert_eq!(complete["errors"], json!([]));
    }

    #[tokio::test]
    async fn notes_search_preserves_diagnostics_on_cursor_pages() {
        let temp = tempfile::tempdir().unwrap();
        let notes = tool(&temp, false);
        let root = temp.path().join(".e-agent/notes");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a"), "hit").unwrap();
        fs::write(root.join("b"), [0xff]).unwrap();
        fs::write(root.join("b-large"), vec![b'x'; READ_LIMIT + 1]).unwrap();
        fs::write(root.join("c"), "hit").unwrap();

        let first = output(
            notes
                .execute(json!({"action":"search","query":"hit","limit":1}))
                .await
                .unwrap(),
        );
        let second = output(
            notes
                .execute(json!({"action":"search","cursor":first["cursor"],"limit":1}))
                .await
                .unwrap(),
        );
        let errors = json!([
            {"path": "b", "reason": "note `b` is not valid UTF-8"},
            {"path": "b-large", "reason": "note `b-large` exceeds the 64 KiB read limit"}
        ]);
        assert_eq!(first["results"], json!(["a:1: hit"]));
        assert_eq!(second["results"], json!(["c:1: hit"]));
        assert_eq!(first["errors"], errors);
        assert_eq!(second["errors"], errors);
        assert_eq!(first["incomplete"], true);
        assert_eq!(second["incomplete"], true);
        assert_eq!(second["has_more"], false);
        assert_eq!(second["cursor"], Value::Null);
    }

    #[tokio::test]
    async fn notes_list_reports_non_utf8_name_without_lossy_access() {
        #[cfg(unix)]
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt;

            let temp = tempfile::tempdir().unwrap();
            let notes = tool(&temp, false);
            let root = temp.path().join(".e-agent/notes");
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("healthy"), "content").unwrap();
            fs::write(root.join("later"), "content").unwrap();
            fs::write(
                root.join(OsString::from_vec(b"bad-\xff".to_vec())),
                "content",
            )
            .unwrap();

            let listed = output(
                notes
                    .execute(json!({"action":"list","limit":1}))
                    .await
                    .unwrap(),
            );
            let continued = output(
                notes
                    .execute(json!({"action":"list","cursor":listed["cursor"],"limit":1}))
                    .await
                    .unwrap(),
            );
            assert_eq!(listed["notes"], json!(["healthy"]));
            assert_eq!(continued["notes"], json!(["later"]));
            assert_eq!(listed["incomplete"], true);
            assert_eq!(listed["errors"], continued["errors"]);
            let path = listed["errors"][0]["path"].as_str().unwrap();
            assert!(path.contains("bad-") && path.contains("\\x"));
            assert_eq!(listed["errors"][0]["reason"], "path is not valid UTF-8");
        }
    }

    #[tokio::test]
    async fn notes_read_bad_note_is_strict_and_names_the_path() {
        let temp = tempfile::tempdir().unwrap();
        let notes = tool(&temp, false);
        let root = temp.path().join(".e-agent/notes");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("bad"), [0xff]).unwrap();

        let error = notes
            .execute(json!({"action":"read","path":"bad"}))
            .await
            .unwrap_err();
        assert!(error.contains("note `bad` is not valid UTF-8"));

        fs::write(root.join("oversized"), vec![b'x'; READ_LIMIT + 1]).unwrap();
        let error = notes
            .execute(json!({"action":"read","path":"oversized"}))
            .await
            .unwrap_err();
        assert!(error.contains("note `oversized` exceeds the 64 KiB read limit"));
    }

    #[tokio::test]
    async fn notes_read_only_and_missing_root_are_safe() {
        let temp = tempfile::tempdir().unwrap();
        let notes = tool(&temp, true);
        let empty = output(notes.execute(json!({"action":"list"})).await.unwrap());
        assert_eq!(
            empty,
            json!({"notes":[],"has_more":false,"cursor":null,"incomplete":false,"errors":[]})
        );
        assert!(
            notes
                .execute(json!({"action":"write","path":"x","content":"x"}))
                .await
                .is_err()
        );
        assert!(!temp.path().join(".e-agent").exists());
    }
}
