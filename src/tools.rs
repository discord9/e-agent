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
    /// Whether `background: true` was explicitly set.
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
        self.running.lock().unwrap().push(RunningTask {
            id,
            label: label.clone(),
            role,
            process_group: process_group.unwrap_or_else(|| Arc::new(AtomicI32::new(0))),
            handle: Arc::new(tokio::spawn(async {})),
            output: None,
            display_meta,
        });
        on_id(id);
        let running = self.running.clone();
        let handle = tokio::spawn(async move {
            let output = work().await;
            running.lock().unwrap().retain(|task| task.id != id);
            on_complete(id, output);
        });
        self.running
            .lock()
            .unwrap()
            .iter_mut()
            .find(|task| task.id == id)
            .unwrap()
            .handle = Arc::new(handle);
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
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    fn web_search(endpoint: String) -> WebSearch {
        WebSearch::for_test("test-api-key".into(), endpoint, Duration::from_secs(1))
    }

    fn http_response(status: &str, body: impl AsRef<[u8]>) -> Vec<u8> {
        let body = body.as_ref();
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    fn redirect_response(location: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .into_bytes()
    }

    async fn read_http_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        let header_end = loop {
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0, "client closed before completing its request");
            request.extend_from_slice(&buffer[..count]);
            if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = std::str::from_utf8(&request[..header_end]).unwrap();
        let content_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length: "))
            .unwrap()
            .parse::<usize>()
            .unwrap();
        while request.len() < header_end + content_length {
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0, "client closed before sending its request body");
            request.extend_from_slice(&buffer[..count]);
        }
        request
    }

    async fn web_server(
        response: Vec<u8>,
        delay: Duration,
    ) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/context", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut socket).await;
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let _ = socket.write_all(&response).await;
            request
        });
        (endpoint, task)
    }

    #[test]
    fn web_search_spec_exposes_only_query() {
        assert_eq!(
            WebSearch::new("key".into()).spec(),
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
        );
    }

    #[test]
    fn web_search_registration_requires_a_nonempty_key() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(temp.path()).unwrap();
        let local_tools = vec![
            "read_file".to_string(),
            "write_file".to_string(),
            "edit_file".to_string(),
            "get_background_tasks".to_string(),
            "bash".to_string(),
        ];
        for key in [None, Some("   ".into())] {
            let (tools, _) = builtins_with_exa_key(workspace.clone(), key, None);
            let names: Vec<String> = tools.iter().map(|tool| tool.spec().name).collect();
            assert_eq!(names, local_tools);
        }
        let (tools, _) = builtins_with_exa_key(workspace, Some(" key ".into()), None);
        let names: Vec<String> = tools.iter().map(|tool| tool.spec().name).collect();
        assert_eq!(
            names,
            [
                "read_file",
                "write_file",
                "edit_file",
                "get_background_tasks",
                "bash",
                "web_search"
            ]
            .map(String::from)
        );
    }

    #[tokio::test]
    async fn web_search_sends_the_expected_request_and_returns_response() {
        let (endpoint, server) = web_server(
            http_response(
                "200 OK",
                br#"{"response":"public test-api-key context","requestId":"id"}"#,
            ),
            Duration::ZERO,
        )
        .await;
        let result = web_search(endpoint)
            .execute(json!({"query": "  Rust ownership docs  "}))
            .await
            .unwrap();
        assert_eq!(result, "public [redacted] context");

        let request = server.await.unwrap();
        let header_end = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4;
        let headers = std::str::from_utf8(&request[..header_end]).unwrap();
        assert!(headers.starts_with("POST /context HTTP/1.1\r\n"));
        assert!(headers.contains("x-api-key: test-api-key\r\n"));
        assert!(headers.contains("content-type: application/json\r\n"));
        let body: Value = serde_json::from_slice(&request[header_end..]).unwrap();
        assert_eq!(
            body,
            json!({"query": "Rust ownership docs", "tokensNum": 5000})
        );
    }

    #[tokio::test]
    async fn web_search_validates_before_connecting() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/context", listener.local_addr().unwrap());
        let tool = web_search(endpoint.clone());
        for arguments in [
            json!({"query": ""}),
            json!({"query": "   "}),
            json!({"query": 7}),
            json!({"query": "界".repeat(WEB_SEARCH_QUERY_LIMIT + 1)}),
        ] {
            assert!(tool.execute(arguments).await.is_err());
        }
        let invalid_key_tool =
            WebSearch::for_test("invalid\nkey".into(), endpoint, Duration::from_secs(1));
        let error = invalid_key_tool
            .execute(json!({"query": "public query"}))
            .await
            .unwrap_err();
        assert_eq!(error, "web search API key is invalid");
        assert!(!error.contains("invalid\nkey"));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn web_search_provider_errors_are_bounded_and_redact_the_key() {
        for status in [
            "401 Unauthorized",
            "402 Payment Required",
            "429 Too Many Requests",
            "500 Internal Server Error",
        ] {
            let (endpoint, server) = web_server(
                http_response(
                    status,
                    format!("provider error for test-api-key ({status})"),
                ),
                Duration::ZERO,
            )
            .await;
            let error = web_search(endpoint)
                .execute(json!({"query": "public query"}))
                .await
                .unwrap_err();
            assert!(error.contains(status.split_whitespace().next().unwrap()));
            assert!(error.contains("provider error"));
            assert!(!error.contains("test-api-key"));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn web_search_does_not_follow_redirects() {
        let second = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let location = format!("http://{}/context", second.local_addr().unwrap());
        let (endpoint, first) = web_server(redirect_response(&location), Duration::ZERO).await;

        let error = web_search(endpoint)
            .execute(json!({"query": "public query"}))
            .await
            .unwrap_err();
        assert!(error.contains("302"));
        first.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), second.accept())
                .await
                .is_err(),
            "redirect target received a request"
        );
    }

    #[tokio::test]
    async fn web_search_handles_malformed_missing_and_unavailable_responses() {
        for body in [
            br#"{"response":"unterminated"#.as_slice(),
            br#"{}"#.as_slice(),
        ] {
            let (endpoint, server) =
                web_server(http_response("200 OK", body), Duration::ZERO).await;
            assert!(
                web_search(endpoint)
                    .execute(json!({"query": "public query"}))
                    .await
                    .unwrap_err()
                    .contains("malformed JSON or no response")
            );
            server.await.unwrap();
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/context", listener.local_addr().unwrap());
        drop(listener);
        assert_eq!(
            web_search(endpoint)
                .execute(json!({"query": "public query"}))
                .await
                .unwrap_err(),
            "web search request failed"
        );

        let (endpoint, server) = web_server(
            http_response("200 OK", br#"{"response":"late"}"#),
            Duration::from_millis(250),
        )
        .await;
        let timeout_tool =
            WebSearch::for_test("test-api-key".into(), endpoint, Duration::from_millis(25));
        assert_eq!(
            timeout_tool
                .execute(json!({"query": "public query"}))
                .await
                .unwrap_err(),
            "web search request failed"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn web_search_caps_success_and_error_bodies() {
        let response = json!({"response": "界".repeat(OUTPUT_LIMIT / 2)}).to_string();
        let (endpoint, server) =
            web_server(http_response("200 OK", response), Duration::ZERO).await;
        let result = web_search(endpoint)
            .execute(json!({"query": "public query"}))
            .await
            .unwrap();
        assert!(result.len() <= OUTPUT_LIMIT);
        assert!(result.is_char_boundary(result.len()));
        assert!(result.ends_with("\n...[truncated]"));
        server.await.unwrap();

        let response = json!({"response": "x".repeat(WEB_SEARCH_RESPONSE_LIMIT * 2)}).to_string();
        let (endpoint, server) =
            web_server(http_response("200 OK", response), Duration::ZERO).await;
        assert!(
            web_search(endpoint)
                .execute(json!({"query": "public query"}))
                .await
                .unwrap_err()
                .contains("response body exceeds")
        );
        server.await.unwrap();

        let (endpoint, server) = web_server(
            http_response(
                "500 Internal Server Error",
                "e".repeat(WEB_SEARCH_ERROR_PREVIEW_LIMIT * 2),
            ),
            Duration::ZERO,
        )
        .await;
        let error = web_search(endpoint)
            .execute(json!({"query": "public query"}))
            .await
            .unwrap_err();
        assert!(
            error.len()
                <= "web search failed with status 500 Internal Server Error: ".len()
                    + WEB_SEARCH_ERROR_PREVIEW_LIMIT
        );
        assert!(error.ends_with("\n...[truncated]"));
        server.await.unwrap();
    }

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
            capture(reader, None).await.unwrap()
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
            background: BackgroundTasks::new(Duration::from_secs(30 * 60), None),
            sandbox: None,
            protect_git: false,
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

    #[test]
    fn bash_description_explains_the_sandbox_only_when_enabled() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(temp.path()).unwrap();
        let background = BackgroundTasks::new(Duration::from_secs(30), None);

        // No sandbox: plain description, no sandbox caveats.
        let plain = Bash {
            workspace: workspace.clone(),
            timeout: Duration::from_secs(1),
            background: background.clone(),
            sandbox: None,
            protect_git: false,
        };
        let plain_desc = plain.spec().description;
        assert!(!plain_desc.contains("sandbox"), "{plain_desc}");

        // Sandboxed with writable workspace AND protect_git = true (subagent).
        let sandboxed = Bash {
            workspace: workspace.clone(),
            timeout: Duration::from_secs(1),
            background: background.clone(),
            sandbox: Some(crate::config::Sandbox {
                enabled: true,
                network: false,
                workspace_writable: true,
                writable_paths: vec!["/mnt/big/cargo-home".into()],
                readable_paths: vec!["~/.rustup".into()],
            }),
            protect_git: true,
        };
        let desc = sandboxed.spec().description;
        assert!(desc.contains("bubblewrap sandbox"), "{desc}");
        assert!(desc.contains("workspace is writable"), "{desc}");
        assert!(desc.contains("OUTSIDE the sandbox"), "{desc}");
        assert!(desc.contains("network is disabled"), "{desc}");
        assert!(desc.contains("/mnt/big/cargo-home"), "{desc}");
        assert!(desc.contains("~/.rustup"), "{desc}");
        assert!(desc.contains("read_file/write_file/edit_file"), "{desc}");
        assert!(desc.contains("`.git`"), "{desc}");
        assert!(desc.contains("linked-worktree pointer"), "{desc}");
        assert!(desc.contains("read-only to prevent"), "{desc}");

        // Sandboxed with read-only workspace, protect_git = false (main).
        let sandboxed_ro = Bash {
            workspace: workspace.clone(),
            timeout: Duration::from_secs(1),
            background: background.clone(),
            sandbox: Some(crate::config::Sandbox {
                enabled: true,
                network: true,
                workspace_writable: false,
                writable_paths: vec![],
                readable_paths: vec![],
            }),
            protect_git: false,
        };
        let desc_ro = sandboxed_ro.spec().description;
        assert!(desc_ro.contains("workspace is read-only"), "{desc_ro}");
        // Use a precise marker: the protect-git text includes backtick-wrapped `.git`.
        assert!(
            !desc_ro.contains("`.git`"),
            "main agent description must not claim .git is read-only: {desc_ro}"
        );

        // Sandboxed with writable workspace, protect_git = false (main):
        // must NOT mention .git.
        let sandboxed_main = Bash {
            workspace,
            timeout: Duration::from_secs(1),
            background,
            sandbox: Some(crate::config::Sandbox {
                enabled: true,
                network: true,
                workspace_writable: true,
                writable_paths: vec![],
                readable_paths: vec![],
            }),
            protect_git: false,
        };
        let desc_main = sandboxed_main.spec().description;
        assert!(
            !desc_main.contains("`.git`"),
            "main agent description must not claim .git is read-only: {desc_main}"
        );
    }

    fn background_bash(
        temp: &tempfile::TempDir,
        timeout: Duration,
    ) -> (Bash, tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut bash = Bash {
            workspace: Workspace::new(temp.path()).unwrap(),
            timeout: Duration::from_secs(30),
            background: BackgroundTasks::new(timeout, None),
            sandbox: None,
            protect_git: false,
        };
        bash.set_event_sender(sender);
        (bash, receiver)
    }

    fn sandbox() -> Option<crate::config::Sandbox> {
        // Only run when bwrap is actually installed and user namespaces work.
        bwrap_available().then(|| crate::config::Sandbox {
            enabled: true,
            network: true,
            workspace_writable: true,
            writable_paths: Vec::new(),
            readable_paths: Vec::new(),
        })
    }

    #[tokio::test]
    async fn sandbox_allows_workspace_writes_but_not_outside() {
        let Some(sandbox) = sandbox() else {
            eprintln!("bwrap unavailable; skipping sandbox test");
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let tool = Bash {
            workspace: Workspace::new(temp.path()).unwrap(),
            timeout: Duration::from_secs(10),
            background: BackgroundTasks::new(Duration::from_secs(30), None),
            sandbox: Some(sandbox),
            protect_git: false,
        };
        // Writing inside the workspace succeeds.
        tool.execute(json!({"command": "echo hi > inside.txt"}))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(temp.path().join("inside.txt")).unwrap(),
            "hi\n"
        );
        // Writing outside the workspace (/tmp is a fresh tmpfs, /usr is ro)
        // must not touch the host: /usr is read-only inside the sandbox.
        let result = tool
            .execute(json!({"command": "touch /usr/e_agent_sandbox_escape 2>&1"}))
            .await;
        assert!(result.is_err(), "write to /usr should fail inside sandbox");
        assert!(!std::path::Path::new("/usr/e_agent_sandbox_escape").exists());
    }

    #[tokio::test]
    async fn sandbox_can_disable_network() {
        let Some(mut sandbox) = sandbox() else {
            eprintln!("bwrap unavailable; skipping sandbox test");
            return;
        };
        sandbox.network = false;
        let temp = tempfile::tempdir().unwrap();
        let tool = Bash {
            workspace: Workspace::new(temp.path()).unwrap(),
            timeout: Duration::from_secs(10),
            background: BackgroundTasks::new(Duration::from_secs(30), None),
            sandbox: Some(sandbox),
            protect_git: false,
        };
        // No loopback either in a fresh net namespace: connecting anywhere fails.
        let result = tool
            .execute(json!({"command": "exec 3<>/dev/tcp/127.0.0.1/80 && echo NET_OK || echo NET_BLOCKED"}))
            .await
            .unwrap();
        assert!(result.contains("NET_BLOCKED"), "{result}");
    }

    #[tokio::test]
    async fn sandbox_extra_writable_and_readable_paths() {
        let Some(mut sandbox) = sandbox() else {
            eprintln!("bwrap unavailable; skipping sandbox test");
            return;
        };
        // A "cache" dir outside the workspace (like ~/.cargo or a shared
        // target disk) and a read-only data dir.
        let cache = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(data.path().join("info.txt"), "data").unwrap();
        sandbox.writable_paths = vec![cache.path().to_string_lossy().into_owned()];
        sandbox.readable_paths = vec![data.path().to_string_lossy().into_owned()];

        let temp = tempfile::tempdir().unwrap();
        let tool = Bash {
            workspace: Workspace::new(temp.path()).unwrap(),
            timeout: Duration::from_secs(10),
            background: BackgroundTasks::new(Duration::from_secs(30), None),
            sandbox: Some(sandbox),
            protect_git: false,
        };
        let cache_path = cache.path().to_string_lossy().into_owned();
        let data_path = data.path().to_string_lossy().into_owned();
        // Writable path: read + write.
        tool.execute(json!({"command": format!("echo cached > '{cache_path}/entry'")}))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(cache.path().join("entry")).unwrap(),
            "cached\n"
        );
        // Readable path: readable but NOT writable.
        let out = tool
            .execute(json!({"command": format!("cat '{data_path}/info.txt'")}))
            .await
            .unwrap();
        assert!(out.contains("data"), "{out}");
        let write = tool
            .execute(json!({"command": format!("touch '{data_path}/nope' 2>&1")}))
            .await;
        assert!(write.is_err(), "readable path must reject writes");
        assert!(!data.path().join("nope").exists());
    }

    #[tokio::test]
    async fn sandbox_protects_workspace_git_directory() {
        let Some(sandbox) = sandbox() else {
            eprintln!("bwrap unavailable; skipping sandbox test");
            return;
        };
        // Create a workspace with a real-looking .git directory.
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        std::fs::write(temp.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let tool = Bash {
            workspace: Workspace::new(temp.path()).unwrap(),
            timeout: Duration::from_secs(10),
            background: BackgroundTasks::new(Duration::from_secs(30), None),
            sandbox: Some(sandbox),
            protect_git: true,
        };
        // Reading .git/HEAD must succeed (git commands read metadata).
        let out = tool
            .execute(json!({"command": "cat .git/HEAD"}))
            .await
            .unwrap();
        assert!(out.contains("ref: refs/heads/main"), "{out}");
        // Writing to any file under .git must fail (read-only bind).
        let write = tool
            .execute(json!({"command": "echo corrupted > .git/HEAD 2>&1"}))
            .await;
        assert!(write.is_err(), "write to .git must be rejected: {write:?}");
        // Removing the .git directory must also fail.
        let rm = tool.execute(json!({"command": "rm -rf .git 2>&1"})).await;
        assert!(rm.is_err(), "rm -rf .git must be rejected: {rm:?}");
        // The host .git must be untouched.
        assert_eq!(
            std::fs::read_to_string(temp.path().join(".git/HEAD")).unwrap(),
            "ref: refs/heads/main\n"
        );
    }

    #[tokio::test]
    async fn sandbox_protects_workspace_git_file_linked_worktree() {
        let Some(sandbox) = sandbox() else {
            eprintln!("bwrap unavailable; skipping sandbox test");
            return;
        };
        // Simulate a linked-worktree worktree: .git is a file containing
        // a gitdir pointer to the main repo (outside the sandbox).
        let temp = tempfile::tempdir().unwrap();
        let gitdir = "/some/external/main/.git/worktrees/feature";
        std::fs::write(temp.path().join(".git"), format!("gitdir: {gitdir}\n")).unwrap();
        let tool = Bash {
            workspace: Workspace::new(temp.path()).unwrap(),
            timeout: Duration::from_secs(10),
            background: BackgroundTasks::new(Duration::from_secs(30), None),
            sandbox: Some(sandbox),
            protect_git: true,
        };
        // Reading the .git pointer must succeed.
        let out = tool.execute(json!({"command": "cat .git"})).await.unwrap();
        assert!(out.contains(gitdir), "{out}");
        // Overwriting or deleting the .git file must fail.
        let write = tool
            .execute(json!({"command": "echo 'gitdir: /evil' > .git 2>&1"}))
            .await;
        assert!(
            write.is_err(),
            "overwrite .git pointer must be rejected: {write:?}"
        );
        let rm = tool.execute(json!({"command": "rm -f .git 2>&1"})).await;
        assert!(rm.is_err(), "rm .git pointer must be rejected: {rm:?}");
        // Verify the host .git pointer is intact.
        assert_eq!(
            std::fs::read_to_string(temp.path().join(".git")).unwrap(),
            format!("gitdir: {gitdir}\n")
        );
    }

    #[tokio::test]
    async fn sandbox_mounts_systemd_resolve_when_present() {
        let Some(sandbox) = sandbox() else {
            eprintln!("bwrap unavailable; skipping sandbox test");
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let tool = Bash {
            workspace: Workspace::new(temp.path()).unwrap(),
            timeout: Duration::from_secs(10),
            background: BackgroundTasks::new(Duration::from_secs(30), None),
            sandbox: Some(sandbox),
            protect_git: false,
        };
        // Check whether /run/systemd/resolve exists on this host.
        let host_has_resolve = std::path::Path::new("/run/systemd/resolve").exists();
        // Inside the sandbox /run should be visible only when systemd-resolve
        // was mounted. The directory itself should either exist and be readable
        // or not exist at all.
        let out = tool
            .execute(
                json!({"command": "test -d /run/systemd/resolve && echo PRESENT || echo ABSENT"}),
            )
            .await
            .unwrap();
        if host_has_resolve {
            assert!(
                out.contains("PRESENT"),
                "/run/systemd/resolve should be mounted when host has it; output: {out}"
            );
            // The stub-resolv.conf should also be readable.
            let contents = tool
                .execute(
                    json!({"command": "cat /run/systemd/resolve/stub-resolv.conf 2>&1 || true"}),
                )
                .await
                .unwrap();
            assert!(
                contents.contains("nameserver"),
                "stub-resolv.conf should contain a nameserver; output: {contents}"
            );
        } else {
            assert!(
                out.contains("ABSENT"),
                "/run/systemd/resolve should NOT exist when host lacks it; output: {out}"
            );
        }
    }

    #[tokio::test]
    async fn sandbox_cat_etc_resolv_conf_works() {
        let Some(sandbox) = sandbox() else {
            eprintln!("bwrap unavailable; skipping sandbox test");
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let tool = Bash {
            workspace: Workspace::new(temp.path()).unwrap(),
            timeout: Duration::from_secs(10),
            background: BackgroundTasks::new(Duration::from_secs(30), None),
            sandbox: Some(sandbox),
            protect_git: false,
        };
        // /etc/resolv.conf is always mounted (--ro-bind-try). Its contents
        // depend on the host config; we just check it is readable.
        let out = tool
            .execute(json!({"command": "cat /etc/resolv.conf 2>&1 || true"}))
            .await
            .unwrap();
        if std::path::Path::new("/etc/resolv.conf").exists() {
            // On hosts with systemd-resolved the symlink target may or may not
            // be reachable. We only assert the file itself is present (the
            // symlink target is covered by sandbox_mounts_systemd_resolve).
            assert!(
                !out.contains("No such file"),
                "/etc/resolv.conf should be readable; output: {out}"
            );
        } else {
            // Host without /etc/resolv.conf (unusual) – skip assertion.
            eprintln!("host has no /etc/resolv.conf; skipping content check");
        }
    }

    #[tokio::test]
    async fn sandbox_dns_resolution_live_smoke() {
        let Some(sandbox) = sandbox() else {
            eprintln!("bwrap unavailable; skipping sandbox test");
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let tool = Bash {
            workspace: Workspace::new(temp.path()).unwrap(),
            timeout: Duration::from_secs(15),
            background: BackgroundTasks::new(Duration::from_secs(30), None),
            sandbox: Some(sandbox),
            protect_git: false,
        };
        // Live DNS resolution: this is a network-dependent smoke test.
        // Skip if /run/systemd/resolve does not exist on the host (no
        // systemd-resolved) or if basic connectivity prerequisites are
        // absent. Use getent hosts (glibc) which respects /etc/nsswitch.conf.
        let host_has_resolve = std::path::Path::new("/run/systemd/resolve").exists();
        if !host_has_resolve {
            eprintln!("host has no /run/systemd/resolve; skipping live DNS smoke test");
            return;
        }
        // First verify that the resolver stub is reachable inside the sandbox.
        let stub_ok = tool
            .execute(json!({"command": "cat /run/systemd/resolve/stub-resolv.conf 2>&1"}))
            .await;
        match stub_ok {
            Ok(out) if out.contains("nameserver") => { /* proceed */ }
            Ok(out) => {
                eprintln!("stub-resolv.conf reachable but no nameserver line: {out}");
                return;
            }
            Err(e) => {
                eprintln!("stub-resolv.conf not reachable inside sandbox: {e}");
                return;
            }
        }
        // Try resolving github.com (commonly available public host).
        let result = tool
            .execute(json!({"command": "getent hosts github.com 2>&1 || nslookup github.com 2>&1 || host github.com 2>&1 || dig +short github.com 2>&1"}))
            .await;
        match result {
            Ok(out) => {
                let trimmed = out.trim();
                if trimmed.is_empty()
                    || trimmed.contains("not found")
                    || trimmed.contains("NXDOMAIN")
                    || trimmed.contains("SERVFAIL")
                {
                    eprintln!("DNS resolution returned no result (external network issue): {out}");
                } else {
                    assert!(
                        trimmed.contains("github.com") || trimmed.contains('.'),
                        "expected resolved address but got: {out}"
                    );
                }
            }
            Err(e) => {
                eprintln!("DNS resolution command failed (external network issue): {e}");
            }
        }
    }

    #[tokio::test]
    async fn completed_background_tasks_leave_the_running_registry() {
        let temp = tempfile::tempdir().unwrap();
        let (bash, mut receiver) = background_bash(&temp, Duration::from_secs(10));
        for id in 1..=8 {
            assert!(
                bash.execute(json!({"command": "true", "background": true}))
                    .await
                    .unwrap()
                    .starts_with(&format!("started background task {id}:"))
            );
        }
        for _ in 0..8 {
            receiver.recv().await.unwrap();
        }
        assert!(bash.background.running().is_empty());
        assert!(
            bash.execute(json!({"command": "true", "background": true}))
                .await
                .unwrap()
                .starts_with("started background task 9:")
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

    #[tokio::test]
    async fn background_task_output_is_visible_while_running() {
        let temp = tempfile::tempdir().unwrap();
        let (bash, _receiver) = background_bash(&temp, Duration::from_secs(10));
        bash.execute(json!({
            "command": "echo hello; sleep 30",
            "background": true
        }))
        .await
        .unwrap();
        let mut saw = String::new();
        for _ in 0..50 {
            let running = bash.background.running();
            assert_eq!(running.len(), 1);
            saw = String::from_utf8_lossy(&running[0].output).into_owned();
            if saw.contains("hello") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(saw.contains("hello"), "output slot never filled: {saw:?}");
    }

    #[tokio::test]
    async fn cancel_kills_the_process_group() {
        let temp = tempfile::tempdir().unwrap();
        let (bash, _receiver) = background_bash(&temp, Duration::from_secs(30));
        bash.execute(json!({
            "command": "sleep 30 & echo $! > child.pid; wait",
            "background": true
        }))
        .await
        .unwrap();
        let id = bash.background.running()[0].id;
        for _ in 0..50 {
            if temp.path().join("child.pid").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let pid = std::fs::read_to_string(temp.path().join("child.pid")).unwrap();
        let label = bash.background.cancel(id).unwrap();
        assert!(label.contains("sleep 30"));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(bash.background.running().is_empty());
        assert!(
            !std::process::Command::new("/bin/kill")
                .arg("-0")
                .arg(pid.trim())
                .status()
                .unwrap()
                .success(),
            "background child survived cancel"
        );
    }

    #[tokio::test]
    async fn get_background_tasks_lists_running_tasks() {
        let temp = tempfile::tempdir().unwrap();
        let (bash, mut receiver) = background_bash(&temp, Duration::from_secs(10));
        let background = bash.background.clone();
        let tool = GetBackgroundTasks {
            background: background.clone(),
        };

        // No tasks running initially.
        assert_eq!(
            tool.execute(json!({})).await.unwrap(),
            "No background tasks running."
        );

        // Start a background task (id=1).
        bash.execute(json!({"command": "echo hello; sleep 30", "background": true}))
            .await
            .unwrap();
        let mut tasks = background.running();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, 1);
        assert_eq!(tasks[0].kind, "bash");

        // The tool reports the running task with correct id, label, and role.
        let output = tool.execute(json!({})).await.unwrap();
        assert_eq!(
            output,
            "1 background task(s) running:\n#1: echo hello; sleep 30 (bash)"
        );

        // Start a second background task (id=2).
        bash.execute(json!({"command": "echo world; sleep 30", "background": true}))
            .await
            .unwrap();
        tasks = background.running();
        assert_eq!(tasks.len(), 2);

        // Both tasks display with their actual ids, not list positions.
        let output = tool.execute(json!({})).await.unwrap();
        assert_eq!(
            output,
            "2 background task(s) running:\n#1: echo hello; sleep 30 (bash)\n#2: echo world; sleep 30 (bash)"
        );

        // Cancel the first task (id=1).
        background.cancel(1);
        let _ = tokio::time::timeout(Duration::from_millis(100), receiver.recv()).await;

        // The remaining task (id=2) still shows as #2, NOT renumbered to #1.
        let output = tool.execute(json!({})).await.unwrap();
        assert_eq!(
            output,
            "1 background task(s) running:\n#2: echo world; sleep 30 (bash)"
        );

        // Cleanup.
        background.cancel(2);
        let _ = tokio::time::timeout(Duration::from_millis(100), receiver.recv()).await;
        assert_eq!(
            tool.execute(json!({})).await.unwrap(),
            "No background tasks running."
        );
    }

    #[tokio::test]
    async fn get_background_tasks_shows_delegate_tasks_as_delegate_not_bash() {
        let temp = tempfile::tempdir().unwrap();
        let (bash, _receiver) = background_bash(&temp, Duration::from_secs(10));
        let background = bash.background.clone();

        let tool = GetBackgroundTasks {
            background: background.clone(),
        };

        // Spawn a delegate-style task (no role, no output slot) via spawn().
        background
            .spawn(
                "search codebase".into(),
                None, // role
                None, // process_group
                || async { "done".into() },
            )
            .unwrap();

        let output = tool.execute(json!({})).await.unwrap();
        // Delegate tasks without a role show their kind ("delegate") instead
        // of being mislabeled as "bash".
        assert_eq!(
            output,
            "1 background task(s) running:\n#1: search codebase (delegate)"
        );

        let tasks = background.running();
        assert_eq!(tasks[0].kind, "delegate");
    }

    #[tokio::test]
    async fn get_background_tasks_shows_roled_delegate_with_role_name() {
        let temp = tempfile::tempdir().unwrap();
        let (bash, _receiver) = background_bash(&temp, Duration::from_secs(10));
        let background = bash.background.clone();

        let tool = GetBackgroundTasks {
            background: background.clone(),
        };

        // A delegate with a known role (e.g. "explorer").
        background
            .spawn(
                "search the logs".into(),
                Some("explorer".into()),
                None,
                || async { "done".into() },
            )
            .unwrap();

        let output = tool.execute(json!({})).await.unwrap();
        assert_eq!(
            output,
            "1 background task(s) running:\n#1: search the logs (explorer)"
        );
    }

    #[tokio::test]
    async fn spawn_with_id_delivers_label_in_background_completed() {
        // Verify that the label registered via spawn_with_id is delivered
        // as BackgroundCompleted.label — sourced from RunningTask, not parsed.
        let temp = tempfile::tempdir().unwrap();
        let (bash, mut receiver) = background_bash(&temp, Duration::from_secs(10));
        let bg = bash.background.clone();
        bg.spawn_with_id(
            "my custom label".into(),
            None,
            None,
            None,
            |_id| {},
            || async { "output from task".into() },
        )
        .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .expect("timeout waiting for completion")
            .unwrap();
        match event {
            AgentEvent::BackgroundCompleted {
                ref label, output, ..
            } => {
                assert_eq!(
                    label.as_deref(),
                    Some("my custom label"),
                    "label must match the registration label"
                );
                assert_eq!(output, "output from task");
            }
            other => panic!("expected BackgroundCompleted, got {other:?}"),
        }
    }

    /// Verify that BackgroundTasks::start propagates protect_git by checking
    /// the parameter is accepted (structural/API coverage). The actual
    /// protect_git effect is verified by the bwrap-based tests below.
    #[tokio::test]
    async fn background_start_accepts_protect_git_parameter() {
        let temp = tempfile::tempdir().unwrap();
        let (bash, mut receiver) = background_bash(&temp, Duration::from_secs(10));

        // Start a background task with protect_git=false (main-agent style).
        bash.execute(json!({"command": "echo main_bg", "background": true}))
            .await
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            event,
            AgentEvent::BackgroundCompleted { output, .. } if output.contains("main_bg")
        ));
    }

    #[tokio::test]
    async fn sandbox_does_not_protect_git_when_protect_git_is_false() {
        let Some(sandbox) = sandbox() else {
            eprintln!("bwrap unavailable; skipping sandbox test");
            return;
        };
        // Create a workspace with a fake .git directory.
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        std::fs::write(temp.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

        // Main agent: protect_git = false → .git is NOT ro-bind.
        let tool = Bash {
            workspace: Workspace::new(temp.path()).unwrap(),
            timeout: Duration::from_secs(10),
            background: BackgroundTasks::new(Duration::from_secs(30), None),
            sandbox: Some(sandbox),
            protect_git: false,
        };
        // Writing to .git/HEAD must succeed (main agent orchestrates git).
        let write = tool
            .execute(json!({"command": "echo 'ref: refs/heads/feature' > .git/HEAD 2>&1; cat .git/HEAD"}))
            .await
            .unwrap();
        assert!(
            write.contains("ref: refs/heads/feature"),
            "main agent must be able to write into .git: {write}"
        );
        // The host .git was actually updated.
        assert_eq!(
            std::fs::read_to_string(temp.path().join(".git/HEAD")).unwrap(),
            "ref: refs/heads/feature\n"
        );
    }

    #[tokio::test]
    async fn sandbox_does_not_protect_git_file_when_protect_git_is_false() {
        let Some(sandbox) = sandbox() else {
            eprintln!("bwrap unavailable; skipping sandbox test");
            return;
        };
        // Simulate a linked-worktree .git pointer file.
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join(".git"),
            "gitdir: /some/other/.git/worktrees/x\n",
        )
        .unwrap();

        // Main agent: protect_git = false → .git pointer is writable.
        let tool = Bash {
            workspace: Workspace::new(temp.path()).unwrap(),
            timeout: Duration::from_secs(10),
            background: BackgroundTasks::new(Duration::from_secs(30), None),
            sandbox: Some(sandbox),
            protect_git: false,
        };
        // Overwriting the .git pointer must succeed.
        let write = tool
            .execute(json!({"command": "echo 'gitdir: /new/path' > .git 2>&1; cat .git"}))
            .await
            .unwrap();
        assert!(
            write.contains("/new/path"),
            "main agent must be able to update .git pointer: {write}"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join(".git")).unwrap(),
            "gitdir: /new/path\n"
        );
    }

    #[tokio::test]
    async fn background_bash_inherits_protect_git_from_parent_bash() {
        let Some(sandbox) = sandbox() else {
            eprintln!("bwrap unavailable; skipping sandbox test");
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        std::fs::write(temp.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

        // Bash with protect_git = true (subagent style).
        // BackgroundTasks must share the same sandbox so bwrap is used.
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut bash = Bash {
            workspace: Workspace::new(temp.path()).unwrap(),
            timeout: Duration::from_secs(10),
            background: BackgroundTasks::new(Duration::from_secs(30 * 60), Some(sandbox.clone())),
            sandbox: Some(sandbox),
            protect_git: true,
        };
        bash.set_event_sender(sender);

        // Start background task: it inherits protect_git=true from bash.
        // Writing to .git/HEAD should fail because the background bash
        // also has .git bound read-only.
        let start = bash
            .execute(json!({"command": "echo corrupted > .git/HEAD 2>&1", "background": true}))
            .await
            .unwrap();
        assert!(start.starts_with("started background task"), "{start}");
        // Give it time to run and fail.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // The host .git must be untouched.
        assert_eq!(
            std::fs::read_to_string(temp.path().join(".git/HEAD")).unwrap(),
            "ref: refs/heads/main\n",
            "background bash with protect_git=true must not corrupt .git"
        );
    }
}
