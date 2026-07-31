use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::HeaderValue;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::agent::{AgentEvent, Tool, ToolSpec, preview};
use crate::workspace::Workspace;

const READ_LIMIT: usize = 64 * 1024;
const DEFAULT_READ_LINES: usize = 2000;
const OUTPUT_LIMIT: usize = 64 * 1024;
const WEB_SEARCH_ENDPOINT: &str = "https://api.exa.ai/context";
const WEB_SEARCH_TIMEOUT: Duration = Duration::from_secs(30);
const WEB_SEARCH_QUERY_LIMIT: usize = 2000;
const WEB_SEARCH_TOKENS: u16 = 5000;
const WEB_SEARCH_ERROR_PREVIEW_LIMIT: usize = 8 * 1024;
const WEB_SEARCH_RESPONSE_LIMIT: usize = 128 * 1024;

/// Check whether bwrap is installed and user namespaces work.
/// Used at startup and by sandbox tests to skip when unavailable.
pub fn bwrap_available() -> bool {
    std::process::Command::new("bwrap")
        .args(["--unshare-all", "--ro-bind", "/", "/", "/bin/true"])
        .status()
        .ok()
        .is_some_and(|status| status.success())
}

/// Built-in tools plus the shared background-task registry, exposed so that
/// other tools (e.g. delegate) can schedule background work too.
pub fn builtins(
    workspace: Workspace,
    sandbox: Option<crate::config::Sandbox>,
) -> (Vec<Box<dyn Tool>>, BackgroundTasks) {
    builtins_with_exa_key(workspace, std::env::var("EXA_API_KEY").ok(), sandbox)
}

/// Builtin tools bound to an existing background-task registry.
///
/// Subagents use this path so their bash completions reach the parent while
/// retaining the same optional web-search capability as the main agent.
pub fn builtins_with_background(
    workspace: Workspace,
    background: BackgroundTasks,
    sandbox: Option<crate::config::Sandbox>,
) -> Vec<Box<dyn Tool>> {
    // Subagent / fixer: protect_git = true so .git is read-only.
    tools_with_background_and_exa_key(
        workspace,
        background,
        std::env::var("EXA_API_KEY").ok(),
        sandbox,
        true,
    )
}

fn builtins_with_exa_key(
    workspace: Workspace,
    exa_api_key: Option<String>,
    sandbox: Option<crate::config::Sandbox>,
) -> (Vec<Box<dyn Tool>>, BackgroundTasks) {
    let background = BackgroundTasks::new(Duration::from_secs(30 * 60), sandbox.clone());
    // Main agent: protect_git = false so git worktree/add/commit work.
    let tools = tools_with_background_and_exa_key(
        workspace,
        background.clone(),
        exa_api_key,
        sandbox,
        false,
    );
    (tools, background)
}

fn tools_with_background_and_exa_key(
    workspace: Workspace,
    background: BackgroundTasks,
    exa_api_key: Option<String>,
    sandbox: Option<crate::config::Sandbox>,
    protect_git: bool,
) -> Vec<Box<dyn Tool>> {
    let mut tools = file_tools(&workspace);
    tools.push(Box::new(GetBackgroundTasks {
        background: background.clone(),
    }));
    tools.push(Box::new(CancelBackgroundTask {
        background: background.clone(),
    }));
    tools.push(bash_tool(workspace, background, sandbox, protect_git));
    if let Some(key) = exa_api_key
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty())
    {
        tools.push(Box::new(WebSearch::new(key)));
    }
    tools
}

/// File tools only; the bash tool is added by the caller so it can be
/// bound to shared [`BackgroundTasks`] (subagents share the parent's
/// registry so their background completions reach the parent agent).
pub fn file_tools(workspace: &Workspace) -> Vec<Box<dyn Tool>> {
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
    ]
}

/// A bash tool bound to a shared background-task registry.
/// `protect_git`: when true, `<workspace>/.git` is bound read-only
/// (subagent / fixer); main agent passes false so orchestration works.
pub fn bash_tool(
    workspace: Workspace,
    background: BackgroundTasks,
    sandbox: Option<crate::config::Sandbox>,
    protect_git: bool,
) -> Box<dyn Tool> {
    Box::new(Bash {
        workspace,
        timeout: Duration::from_secs(30),
        background,
        sandbox,
        protect_git,
    })
}

struct GetBackgroundTasks {
    background: BackgroundTasks,
}

#[async_trait]
impl Tool for GetBackgroundTasks {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "get_background_tasks".into(),
            description: "Return a one-time snapshot of currently running background tasks. \
                Do not poll or repeatedly call this tool to wait for completion. \
                Results are delivered automatically as [background task N completed]; \
                continue independent work or end the turn while waiting."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }

    async fn execute(&self, _arguments: Value) -> Result<String, String> {
        let tasks = self.background.running();
        if tasks.is_empty() {
            return Ok("No background tasks running.".into());
        }
        let mut out = format!("{} background task(s) running:\n", tasks.len());
        for task in tasks.iter() {
            let role = task.role.as_deref().unwrap_or(&task.kind);
            let tags = task
                .display_meta
                .as_ref()
                .map(|meta| {
                    let mut t = String::new();
                    if meta.background {
                        t.push_str(" [background]");
                    }
                    if let Some(ws) = &meta.workspace {
                        use crate::agent::preview;
                        t.push_str(&format!(" [workspace: {}]", preview(ws, 40)));
                    }
                    t
                })
                .unwrap_or_default();
            out.push_str(&format!(
                "#{}: {} ({}){}\n",
                task.id, task.label, role, tags
            ));
        }
        out.truncate(out.trim_end().len());
        Ok(out)
    }
}

struct CancelBackgroundTask {
    background: BackgroundTasks,
}

#[async_trait]
impl Tool for CancelBackgroundTask {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "cancel_background_task".into(),
            description: "Cancel a currently running background bash or delegate task.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "background task id"}
                },
                "required": ["id"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<String, String> {
        let id = arguments
            .as_object()
            .ok_or("tool arguments must be a JSON object")?
            .get("id")
            .and_then(Value::as_u64)
            .ok_or("`id` must be a non-negative integer")?;
        self.background
            .cancel(id)
            .ok_or_else(|| format!("background task {id} is not running"))?;
        Ok(format!("cancelled background task {id}"))
    }
}

struct WebSearch {
    api_key: String,
    client: reqwest::Client,
    endpoint: String,
    timeout: Duration,
}

#[derive(Deserialize)]
struct ExaContextResponse {
    response: String,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
}

impl WebSearch {
    fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: web_search_client(),
            endpoint: WEB_SEARCH_ENDPOINT.into(),
            timeout: WEB_SEARCH_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn for_test(api_key: String, endpoint: String, timeout: Duration) -> Self {
        Self {
            api_key,
            client: web_search_client(),
            endpoint,
            timeout,
        }
    }
}

fn web_search_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .expect("web search client configuration is valid")
}

#[async_trait]
impl Tool for WebSearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_search".into(),
            description: "Search public web documentation and code examples. Never include secrets, private source code, internal URLs, or personal data in the query.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "A specific public-web research query."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<String, String> {
        let query = required_string(&arguments, "query")?.trim();
        if query.is_empty() {
            return Err("`query` must not be empty".into());
        }
        if query.chars().count() > WEB_SEARCH_QUERY_LIMIT {
            return Err(format!(
                "`query` must be at most {WEB_SEARCH_QUERY_LIMIT} characters"
            ));
        }
        let mut api_key: HeaderValue = self
            .api_key
            .parse()
            .map_err(|_| "web search API key is invalid".to_string())?;
        api_key.set_sensitive(true);

        let mut response = self
            .client
            .post(&self.endpoint)
            .timeout(self.timeout)
            .header("x-api-key", api_key)
            .json(&json!({"query": query, "tokensNum": WEB_SEARCH_TOKENS}))
            .send()
            .await
            .map_err(|_| "web search request failed".to_string())?;
        if !response.status().is_success() {
            let status = response.status();
            let (body, truncated) =
                read_response_prefix(&mut response, WEB_SEARCH_ERROR_PREVIEW_LIMIT)
                    .await
                    .map_err(|_| format!("web search failed with status {status}"))?;
            let mut context = truncate_utf8(
                String::from_utf8_lossy(&body).into_owned(),
                WEB_SEARCH_ERROR_PREVIEW_LIMIT,
            );
            if truncated {
                context = truncate_utf8(
                    format!("{context}\n...[truncated]"),
                    WEB_SEARCH_ERROR_PREVIEW_LIMIT,
                );
            }
            let error = if context.is_empty() {
                format!("web search failed with status {status}")
            } else {
                format!("web search failed with status {status}: {context}")
            };
            return Err(redact_api_key(error, &self.api_key));
        }

        let (body, truncated) = read_response_prefix(&mut response, WEB_SEARCH_RESPONSE_LIMIT)
            .await
            .map_err(|_| "web search response body failed".to_string())?;
        if truncated {
            return Err(format!(
                "web search response body exceeds {WEB_SEARCH_RESPONSE_LIMIT} bytes"
            ));
        }
        let context: ExaContextResponse = serde_json::from_slice(&body)
            .map_err(|_| "web search returned malformed JSON or no response".to_string())?;
        let _ = context.request_id;
        Ok(truncate_utf8(
            redact_api_key(context.response, &self.api_key),
            OUTPUT_LIMIT,
        ))
    }
}

async fn read_response_prefix(
    response: &mut reqwest::Response,
    limit: usize,
) -> Result<(Vec<u8>, bool), reqwest::Error> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        let room = limit.saturating_sub(bytes.len());
        if chunk.len() > room {
            bytes.extend_from_slice(&chunk[..room]);
            return Ok((bytes, true));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((bytes, false))
}

fn truncate_utf8(mut text: String, limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }
    let marker = "\n...[truncated]";
    let mut end = limit.saturating_sub(marker.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    if limit >= marker.len() {
        text.push_str(marker);
    }
    text
}

fn redact_api_key(text: String, api_key: &str) -> String {
    text.replace(api_key, "[redacted]")
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
    sandbox: Option<crate::config::Sandbox>,
    /// When true and sandbox is enabled, bind `<workspace>/.git` read-only
    /// so subagents / fixers cannot corrupt the repository metadata.
    protect_git: bool,
}

#[async_trait]
impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        let mut description =
            "Run a shell command with the workspace as its current directory.".to_owned();
        if let Some(sandbox) = &self.sandbox {
            let ws_mode = if sandbox.workspace_writable {
                "writable"
            } else {
                "read-only"
            };
            description.push_str(&format!(
                " The command runs inside a bubblewrap sandbox: the workspace is {ws_mode}, \
                 system dirs (/usr, /bin, /lib, /etc) are read-only, and most of $HOME is \
                 hidden behind a fresh tmpfs. Files that are not mounted — e.g. ~/.ssh or \
                 ~/.gitconfig — fail with \"No such file or directory\"; that means they are \
                 OUTSIDE the sandbox, not that they don't exist on the host. Do not try to \
                 create such files; if a needed path errors this way, tell the user it is \
                 outside the sandbox.",
            ));
            if !sandbox.network {
                description.push_str(" The network is disabled (no DNS, no connections).");
            }
            if !sandbox.writable_paths.is_empty() {
                description.push_str(&format!(
                    " Extra writable paths: {}.",
                    sandbox.writable_paths.join(", ")
                ));
            }
            if !sandbox.readable_paths.is_empty() {
                description.push_str(&format!(
                    " Extra read-only paths: {}.",
                    sandbox.readable_paths.join(", ")
                ));
            }
            description.push_str(
                " The read_file/write_file/edit_file tools are restricted to the workspace \
                 (capability-relative, not the sandbox); to read a file outside the workspace \
                 that is mounted in the sandbox, use bash (e.g. cat).",
            );
            if self.protect_git {
                description.push_str(
                    " The workspace `.git` metadata (directory or linked-worktree pointer) \
                     is bound read-only to prevent accidental corruption by fixer subagents.",
                );
            }
        }
        ToolSpec {
            name: "bash".into(),
            description,
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
            return self.background.start(
                self.workspace.clone(),
                command.to_owned(),
                self.protect_git,
            );
        }
        run_bash(
            &self.workspace,
            command,
            self.timeout,
            self.protect_git,
            None,
            None,
            self.sandbox.as_ref(),
        )
        .await
    }

    fn set_event_sender(&mut self, sender: tokio::sync::mpsc::UnboundedSender<AgentEvent>) {
        self.background.sender = Some(sender);
    }

    fn has_event_sender(&self) -> bool {
        self.background.sender.is_some()
    }
}

#[derive(Clone)]
pub struct BackgroundTasks {
    next_id: Arc<AtomicU64>,
    running: Arc<std::sync::Mutex<Vec<RunningTask>>>,
    sender: Option<tokio::sync::mpsc::UnboundedSender<AgentEvent>>,
    timeout: Duration,
    sandbox: Option<crate::config::Sandbox>,
}

/// A live, read-only snapshot of a background task's combined stdout+stderr
/// tail (capped at 16KB), shared between the running `bash` capture and the
/// TUI panel. Only `BackgroundTasks::start` attaches one.
pub type OutputSlot = Arc<std::sync::Mutex<Vec<u8>>>;

const SLOT_LIMIT: usize = 16 * 1024;

fn slot_append(slot: &OutputSlot, chunk: &[u8]) {
    let mut bytes = slot.lock().unwrap();
    bytes.extend_from_slice(chunk);
    if bytes.len() > SLOT_LIMIT {
        let excess = bytes.len() - SLOT_LIMIT;
        bytes.drain(..excess);
    }
}

#[derive(Clone)]
struct RunningTask {
    id: u64,
    label: String,
    role: Option<String>,
    process_group: Arc<AtomicI32>,
    handle: Arc<tokio::task::JoinHandle<()>>,
    output: Option<OutputSlot>,
    display_meta: Option<TaskDisplayMeta>,
}

/// Structured metadata for delegate-task display in the F2 task panel.
/// Avoids label/string re-parsing. Only populated for delegate tasks.
#[derive(Clone, Debug, Default)]
pub struct TaskDisplayMeta {
    /// Effective delegate execution mode (`true` for background).
    pub background: bool,
    /// Explicit user-provided workspace path (trimmed, non-empty); `None`
    /// means the parent's default workspace was inherited.
    pub workspace: Option<String>,
}

/// A snapshot of one running background task, for display.
#[derive(Clone, Debug)]
pub struct BackgroundTaskInfo {
    pub id: u64,
    pub label: String,
    pub role: Option<String>,
    /// "bash" for background shell commands, "delegate" for subagent tasks.
    pub kind: String,
    /// Combined stdout/stderr tail so far; empty for non-bash tasks.
    pub output: Vec<u8>,
    /// Delegate-specific display metadata; `None` for non-delegate tasks.
    pub display_meta: Option<TaskDisplayMeta>,
}

impl BackgroundTasks {
    fn new(timeout: Duration, sandbox: Option<crate::config::Sandbox>) -> Self {
        Self {
            next_id: Arc::new(AtomicU64::new(1)),
            running: Arc::new(std::sync::Mutex::new(Vec::new())),
            sender: None,
            timeout,
            sandbox,
        }
    }

    /// Set the channel used to deliver background completions. Called by the
    /// agent for tools that hold a shared clone of this registry.
    pub fn set_event_sender(&mut self, sender: tokio::sync::mpsc::UnboundedSender<AgentEvent>) {
        self.sender = Some(sender);
    }

    /// Whether background completion delivery is currently usable.
    ///
    /// This is a read-only preflight; spawning still performs its own final
    /// sender check to guard against the receiver closing in the meantime.
    pub fn completion_delivery_available(&self) -> bool {
        self.sender
            .as_ref()
            .is_some_and(|sender| !sender.is_closed())
    }

    /// Snapshot of currently running background tasks, for the TUI panel.
    pub fn running(&self) -> Vec<BackgroundTaskInfo> {
        self.running
            .lock()
            .unwrap()
            .iter()
            .map(|task| BackgroundTaskInfo {
                id: task.id,
                label: task.label.clone(),
                role: task.role.clone(),
                kind: if task.output.is_some() {
                    "bash".into()
                } else {
                    "delegate".into()
                },
                output: task
                    .output
                    .as_ref()
                    .map(|slot| slot.lock().unwrap().clone())
                    .unwrap_or_default(),
                display_meta: task.display_meta.clone(),
            })
            .collect()
    }

    /// Cancel a running background task. Aborting its future drops any
    /// in-flight `run_bash`, which kills the process group via its guard.
    /// Returns the cancelled task's label, or `None` if no such task.
    pub fn cancel(&self, id: u64) -> Option<String> {
        let task = {
            let mut running = self.running.lock().unwrap();
            let index = running.iter().position(|task| task.id == id)?;
            running.remove(index)
        };
        task.handle.abort();
        Some(task.label)
    }

    /// Start a background bash command. Returns a human-readable "started"
    /// message containing the task id.
    /// `protect_git` controls whether `<workspace>/.git` is bound read-only
    /// inside the sandbox (subagent = true, main agent = false).
    pub fn start(
        &self,
        workspace: Workspace,
        command: String,
        protect_git: bool,
    ) -> Result<String, String> {
        let process_group = Arc::new(AtomicI32::new(0));
        let output: OutputSlot = Arc::new(std::sync::Mutex::new(Vec::new()));
        let pg = process_group.clone();
        let slot = output.clone();
        let timeout = self.timeout;
        let running = self.running.clone();
        let sandbox = self.sandbox.clone();
        self.spawn_with_id(
            preview(&command, 100),
            None,
            Some(process_group),
            None, // display_meta
            move |id| {
                let mut running = running.lock().unwrap();
                if let Some(task) = running.iter_mut().find(|task| task.id == id) {
                    task.output = Some(output);
                }
            },
            move || async move {
                match run_bash(
                    &workspace,
                    &command,
                    timeout,
                    protect_git,
                    Some(pg),
                    Some(slot),
                    sandbox.as_ref(),
                )
                .await
                {
                    Ok(output) | Err(output) => output,
                }
            },
        )
    }

    /// Register and spawn a background future. Completion is delivered as
    /// [`AgentEvent::BackgroundCompleted`]. `process_group` is only used for
    /// kill-on-drop cleanup; pass `None` for non-process tasks.
    pub fn spawn<F, Fut>(
        &self,
        label: String,
        role: Option<String>,
        process_group: Option<Arc<AtomicI32>>,
        work: F,
    ) -> Result<String, String>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = String> + Send + 'static,
    {
        self.spawn_with_id(label, role, process_group, None, |_| {}, work)
    }

    /// Like [`Self::spawn`], but invokes `on_id` with the allocated task id
    /// before the work starts (so callers can register per-task state under
    /// the same id).
    pub fn spawn_with_id<F, Fut>(
        &self,
        label: String,
        role: Option<String>,
        process_group: Option<Arc<AtomicI32>>,
        display_meta: Option<TaskDisplayMeta>,
        on_id: impl FnOnce(u64),
        work: F,
    ) -> Result<String, String>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = String> + Send + 'static,
    {
        let sender = self
            .sender
            .clone()
            .filter(|sender| !sender.is_closed())
            .ok_or("background task delivery is unavailable")?;
        let completion_label = label.clone();
        self.spawn_inner(
            label,
            role,
            process_group,
            display_meta,
            on_id,
            work,
            move |id, output| {
                let _ = sender.send(AgentEvent::BackgroundCompleted {
                    id,
                    output,
                    label: Some(completion_label.clone()),
                });
            },
        )
    }

    /// Spawn a registered task that runs to completion but does NOT send a
    /// completion event. Used by synchronous delegate: the subagent must be
    /// visible in the task panel, but its answer is
    /// returned as the tool result, so a completion notice would duplicate.
    pub fn spawn_silent<F, Fut>(
        &self,
        label: String,
        role: Option<String>,
        process_group: Option<Arc<AtomicI32>>,
        display_meta: Option<TaskDisplayMeta>,
        on_id: impl FnOnce(u64),
        work: F,
    ) -> Result<String, String>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = String> + Send + 'static,
    {
        self.spawn_inner(
            label,
            role,
            process_group,
            display_meta,
            on_id,
            work,
            |_, _| {},
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_inner<F, Fut>(
        &self,
        label: String,
        role: Option<String>,
        process_group: Option<Arc<AtomicI32>>,
        display_meta: Option<TaskDisplayMeta>,
        on_id: impl FnOnce(u64),
        work: F,
        on_complete: impl FnOnce(u64, String) + Send + 'static,
    ) -> Result<String, String>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = String> + Send + 'static,
    {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let started = format!("started background task {id}: {label}");
        let (work_tx, work_rx) = tokio::sync::oneshot::channel::<F>();
        let running = self.running.clone();
        let handle = tokio::spawn(async move {
            let Ok(work) = work_rx.await else {
                running.lock().unwrap().retain(|task| task.id != id);
                return;
            };
            let output = work().await;
            let completed = {
                let mut running = running.lock().unwrap();
                if let Some(index) = running.iter().position(|task| task.id == id) {
                    running.remove(index);
                    true
                } else {
                    false
                }
            };
            if completed {
                on_complete(id, output);
            }
        });
        self.running.lock().unwrap().push(RunningTask {
            id,
            label,
            role,
            process_group: process_group.unwrap_or_else(|| Arc::new(AtomicI32::new(0))),
            handle: Arc::new(handle),
            output: None,
            display_meta,
        });
        on_id(id);
        if let Err(work) = work_tx.send(work) {
            drop(work);
        }
        Ok(started)
    }
}

impl Drop for BackgroundTasks {
    fn drop(&mut self) {
        #[cfg(unix)]
        for task in self.running.lock().unwrap().iter() {
            if let Some(process_group) =
                rustix::process::Pid::from_raw(task.process_group.load(Ordering::Acquire))
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
    protect_git: bool,
    process_group_slot: Option<Arc<AtomicI32>>,
    output_slot: Option<OutputSlot>,
    sandbox: Option<&crate::config::Sandbox>,
) -> Result<String, String> {
    // Build the command: bare bash, or wrapped in bwrap when sandboxed.
    // bwrap is a *construction tool*, so we spell out the policy explicitly:
    // system dirs read-only, workspace writable (per config), /tmp scratch,
    // no new privileges, TIOCSTI blocked, die with the parent. Network is
    // shared by default (agents often need to fetch); config can disable it.
    let mut process = match sandbox {
        Some(sandbox) => {
            let root = workspace.root();
            let workspace_bind = if sandbox.workspace_writable {
                "--bind"
            } else {
                "--ro-bind"
            };
            let root_str = root.to_string_lossy().into_owned();
            // Order matters: extra paths under /home (e.g. ~/.cargo) must be
            // mounted AFTER the /home tmpfs, or the tmpfs would shadow them.
            let mut args: Vec<String> = vec![
                "--dev".into(),
                "/dev".into(),
                "--proc".into(),
                "/proc".into(),
                "--ro-bind".into(),
                "/usr".into(),
                "/usr".into(),
                "--ro-bind".into(),
                "/bin".into(),
                "/bin".into(),
                "--ro-bind".into(),
                "/lib".into(),
                "/lib".into(),
                "--ro-bind".into(),
                "/lib64".into(),
                "/lib64".into(),
                "--ro-bind-try".into(),
                "/etc".into(),
                "/etc".into(),
            ];
            // If systemd-resolved is running on the host, mount its stub
            // resolver so that symlinked /etc/resolv.conf works inside the
            // sandbox. Only mount the resolver directory, not whole /run.
            if std::path::Path::new("/run/systemd/resolve").exists() {
                args.push("--dir".into());
                args.push("/run/systemd".into());
                args.push("--ro-bind".into());
                args.push("/run/systemd/resolve".into());
                args.push("/run/systemd/resolve".into());
            }
            args.extend([
                "--tmpfs".into(),
                "/tmp".into(),
                "--tmpfs".into(),
                "/home".into(),
                // Workspace bind: must come before extra mounts so that an
                // extra path *under* the workspace takes priority (bind-try
                // will mount over the earlier workspace bind).
                workspace_bind.into(),
                root_str.clone(),
                root_str.clone(),
            ]);
            // Extra user-configured mounts (cargo caches, shared target
            // disks, ...). These come after the workspace bind so they are
            // not shadowed by it. --bind-try / --ro-bind-try skip paths that
            // do not exist on the host so a missing cache dir cannot break.
            for path in &sandbox.writable_paths {
                args.push("--bind-try".into());
                args.push(path.clone());
                args.push(path.clone());
            }
            for path in &sandbox.readable_paths {
                args.push("--ro-bind-try".into());
                args.push(path.clone());
                args.push(path.clone());
            }
            // Protect the project-local sandbox config from the agent:
            // bind /dev/null over it so writes are silently discarded.
            // This must come after all other binds (including extra paths)
            // to ensure it is not shadowed.
            let project_config = format!("{}/.e-agent/config.toml", root_str);
            args.push("--ro-bind".into());
            args.push("/dev/null".into());
            args.push(project_config);

            // Protect the workspace .git metadata (directory or
            // linked-worktree pointer file) by binding it read-only over
            // itself. This prevents the fixer / subagent from deleting or
            // corrupting the pointer, running `git init`, or writing any
            // commit metadata. It comes after all writable binds and the
            // .e-agent/config.toml protection so it cannot be shadowed.
            // Only enabled for subagents (protect_git=true); the main agent
            // needs writable .git for orchestration (git add/commit/etc).
            if protect_git {
                let git_path = format!("{root_str}/.git");
                if std::path::Path::new(&git_path).exists() {
                    args.push("--ro-bind".into());
                    args.push(git_path.clone());
                    args.push(git_path);
                }
            }
            args.extend([
                "--unshare-pid".into(),
                "--unshare-ipc".into(),
                "--unshare-uts".into(),
                "--new-session".into(),
                "--die-with-parent".into(),
                "--chdir".into(),
                root_str,
            ]);
            if !sandbox.network {
                args.push("--unshare-net".into());
            }
            args.push("/bin/bash".into());
            args.push("-lc".into());
            args.push(command.to_owned());
            let mut cmd = Command::new("bwrap");
            cmd.args(args);
            // Strip credential env vars so they are not inherited by the
            // sandboxed command. The agent reads these from the parent
            // process env directly, not via bash, so removing them here
            // does not break anything.
            cmd.env_remove("EXA_API_KEY");
            cmd.env_remove("OPENAI_API_KEY");
            cmd.env_remove("ANTHROPIC_API_KEY");
            cmd.env_remove("DEEPSEEK_API_KEY");
            cmd.env_remove("MOONSHOT_API_KEY");
            cmd.env_remove("KIMI_API_KEY");
            cmd
        }
        None => {
            let mut cmd = Command::new("/bin/bash");
            cmd.arg("-lc").arg(command);
            cmd
        }
    };
    process
        .current_dir(workspace.root())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    process.process_group(0);
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) if sandbox.is_some() && error.kind() == std::io::ErrorKind::NotFound => {
            // Fail closed: if bwrap is configured but missing, return the
            // error so the model (or startup check) sees it.
            return Err(format!(
                "bwrap not found: sandbox requires bubblewrap (bwrap) to be installed: {error}"
            ));
        }
        Err(error) => return Err(format!("failed to start shell: {error}")),
    };
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
        let (stdout, stderr, status) = tokio::join!(
            capture(stdout, output_slot.clone()),
            capture(stderr, output_slot),
            child.wait()
        );
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

async fn capture(
    mut reader: impl AsyncRead + Unpin,
    slot: Option<OutputSlot>,
) -> std::io::Result<Captured> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(Captured { bytes, truncated });
        }
        if let Some(slot) = &slot {
            slot_append(slot, &buffer[..count]);
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
#[path = "tools_tests.rs"]
mod tests;
