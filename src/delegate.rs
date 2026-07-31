//! `delegate` tool: spawn a subagent with a fresh context to work on a task.
//!
//! Each subagent runs as an independent [`SessionRunner`] task on the shared
//! tokio runtime with an empty history. Its agent state (history, pending
//! background results, and token counts) is isolated from the parent and
//! exposed through a [`SessionHandle`].
//!
//! The subagent gets the builtin file/bash tools and, when configured, public
//! web search (no MCP tools, no `delegate` itself — depth is capped at 1 by
//! construction). In background mode the answer is delivered as a
//! [`AgentEvent::BackgroundCompleted`] through the parent's event channel,
//! waking an idle agent.
//!
//! Every subagent uses a runner [`SessionHandle`] to queue prompts, request
//! compaction, or cancel its in-flight turn. Process-level isolation through a
//! spawned `e-agent --subagent` remains a planned future evolution.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::{Agent, AgentEvent, Tool, ToolSpec, preview};
use crate::config::SessionBackend;
use crate::model::ConfiguredModel;
use crate::runner::{IdlePolicy, SessionHandle, SessionResult, SessionRunner};
use crate::session_store::SessionStore;
use crate::tools::BackgroundTasks;
use crate::workspace::Workspace;
/// Metadata about a live subagent session, stored alongside its handle in
/// the registry so frontends can display the model name, role, cwd, etc.
pub struct SessionEntry {
    pub handle: SessionHandle,
    pub model: String,
    pub role: Option<String>,
    pub cwd: String,
    pub session_id: String,
    pub context_window: Option<u64>,
}

/// Registry of live session handles, keyed by background-task id (the same
/// unified id sequence shared with background bash). Background subagents
/// register their handle here so the TUI can attach; entries are removed
/// when the subagent finishes.
#[derive(Clone, Default)]
pub struct Sessions {
    sessions: Arc<Mutex<std::collections::HashMap<u64, Arc<SessionEntry>>>>,
}

impl Sessions {
    pub fn get(&self, id: u64) -> Option<Arc<SessionEntry>> {
        self.sessions.lock().unwrap().get(&id).cloned()
    }

    pub fn insert(&self, id: u64, entry: Arc<SessionEntry>) {
        self.sessions.lock().unwrap().insert(id, entry);
    }

    pub fn remove(&self, id: u64) {
        self.sessions.lock().unwrap().remove(&id);
    }
}

/// Cancellation-safe ownership of delegate registration side effects.
///
/// This guard is captured by the scheduler's `FnOnce`, rather than created in
/// its async body, so aborting the wrapper before its first poll still cleans
/// up the session and parent recovery record.
struct DelegateCleanup {
    id: Arc<Mutex<Option<u64>>>,
    sessions: Sessions,
    recovery: Option<crate::session_store::BackgroundRecord>,
}

impl DelegateCleanup {
    fn new(
        id: Arc<Mutex<Option<u64>>>,
        sessions: Sessions,
        recovery: Option<crate::session_store::BackgroundRecord>,
    ) -> Self {
        Self {
            id,
            sessions,
            recovery,
        }
    }

    fn finish(mut self) {
        self.cleanup();
    }

    fn cleanup(&mut self) {
        let id = match self.id.lock() {
            Ok(mut slot) => slot.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        let Some(id) = id else {
            return;
        };
        match self.sessions.sessions.lock() {
            Ok(mut sessions) => {
                sessions.remove(&id);
            }
            Err(poisoned) => {
                poisoned.into_inner().remove(&id);
            }
        }
        if let Some(record) = &self.recovery {
            record
                .store
                .clear_background_task(&record.root, &record.session, id);
        }
    }
}

impl Drop for DelegateCleanup {
    fn drop(&mut self) {
        self.cleanup();
    }
}

pub struct Delegate {
    /// Subagents run on the role-routed model when configured, otherwise
    /// on the main model.
    subagent_model: ConfiguredModel,
    /// Context window for the default subagent model (from profile).
    subagent_context_window: Option<u64>,
    workspace: Workspace,
    /// Shared running-task registry: background delegates and bash commands
    /// stay visible and cancellable together. Delegate wrappers complete to
    /// the parent; each child Bash facade completes to its child Agent.
    background: BackgroundTasks,
    /// Live handles of background subagents, for the TUI attach view.
    sessions: Sessions,
    /// Directory where each subagent persists its own session file
    /// (None = in-memory only, e.g. tests). Files are named after a fresh
    /// unique session id (`sub-<timestamp>-<rand>`), never the background
    /// task id — task ids restart at 1 every process and would collide
    /// across restarts.
    persist_root: Option<std::path::PathBuf>,
    /// Per-role models from `[roles]` (role name -> model). A role not
    /// present here falls back to `subagent_model`.
    role_models: std::collections::HashMap<String, ConfiguredModel>,
    /// Context window per role model (from profile).
    role_context_windows: std::collections::HashMap<String, Option<u64>>,
    /// Workspace root used to read role templates (`.e-agent/agents/<role>.md`).
    roles_root: Option<std::path::PathBuf>,
    /// Optional bwrap sandbox inherited by every subagent's bash tool.
    sandbox: Option<crate::config::Sandbox>,
    /// Parent session's background-task record (workspace root + session
    /// name + the store that owns the record), so subagent delegates are
    /// recorded alongside bash background tasks and trigger the "killed on
    /// exit" notice on restart.
    record_in: Option<crate::session_store::BackgroundRecord>,
    /// Session backend configuration for subagent persistence (not a
    /// connected store — each subagent connects its own).
    persist_backend: SessionBackend,
}

/// Where a subagent writes its own session file.
#[derive(Clone)]
pub struct PersistConfig {
    root: std::path::PathBuf,
    /// This subagent's session id, assigned at spawn time.
    session_id: String,
    /// Session backend configuration. The subagent connects its own store
    /// from this when starting the runner.
    backend: SessionBackend,
}

/// Task-panel title for a delegation: the caller's `label` wins (trimmed,
/// non-empty, capped), then the role name, then a task preview.
fn task_label(label: Option<&str>, role: Option<&str>, task: &str) -> String {
    let single_line = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    label
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(|label| preview(&single_line(label), 60))
        .or_else(|| role.map(single_line))
        .unwrap_or_else(|| preview(&single_line(task), 60))
}

/// A delegated task plus the role template (if any) that shapes the
/// subagent's system prompt.
struct DelegatedTask {
    task: String,
    role_prompt: Option<String>,
    /// True when the role's frontmatter declared `read_only = true` (or the
    /// caller's policy demands it): the subagent gets no write/edit tools and
    /// its bash runs in a narrowed read-only sandbox with network disabled.
    read_only: bool,
    sandbox: Option<crate::config::Sandbox>,
}

impl Delegate {
    pub fn new(model: ConfiguredModel, workspace: Workspace, background: BackgroundTasks) -> Self {
        Self {
            subagent_model: model,
            subagent_context_window: None,
            workspace,
            background,
            sessions: Sessions::default(),
            persist_root: None,
            role_models: std::collections::HashMap::new(),
            role_context_windows: std::collections::HashMap::new(),
            roles_root: None,
            sandbox: None,
            record_in: None,
            persist_backend: SessionBackend::Jsonl,
        }
    }

    /// Set the context window for the default subagent model.
    pub fn with_subagent_context_window(mut self, window: Option<u64>) -> Self {
        self.subagent_context_window = window;
        self
    }

    /// Set context windows for role models.
    pub fn with_role_context_windows(
        mut self,
        windows: std::collections::HashMap<String, Option<u64>>,
    ) -> Self {
        self.role_context_windows = windows;
        self
    }

    /// Route subagents onto a different model (e.g. a cheaper profile from
    /// `[roles] subagent = "…"`). Without this they share the main model.
    pub fn with_subagent_model(mut self, model: ConfiguredModel) -> Self {
        self.subagent_model = model;
        self
    }

    /// Route specific roles onto their own models (`[roles] <role> = "…"`).
    /// A role without an entry uses the subagent (or main) model.
    pub fn with_role_models(
        mut self,
        role_models: std::collections::HashMap<String, ConfiguredModel>,
    ) -> Self {
        self.role_models = role_models;
        self
    }

    /// Enable role templates: the workspace root that holds
    /// `.e-agent/agents/<role>.md`. Without this, `delegate` has no roles.
    pub fn with_roles_root(mut self, root: std::path::PathBuf) -> Self {
        self.roles_root = Some(root);
        self
    }

    /// Sandbox every subagent's bash tool with the same bwrap policy as the
    /// main agent.
    pub fn with_sandbox(mut self, sandbox: Option<crate::config::Sandbox>) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Narrow the inherited bwrap policy for a read-only role: workspace
    /// read-only, extra writable roots dropped, network disabled. `None`
    /// (sandbox disabled / bwrap unavailable) stays `None` — a read-only
    /// role then gets no bash tool at all (fail closed).
    fn read_only_sandbox(&self) -> Option<crate::config::Sandbox> {
        self.sandbox.as_ref().map(crate::tools::read_only_sandbox)
    }

    /// Record subagent background tasks in the parent session's record so a
    /// restart can warn about killed subagents alongside killed bash tasks.
    pub fn record_background_tasks_in(
        mut self,
        root: std::path::PathBuf,
        session: &str,
        store: SessionStore,
    ) -> Self {
        self.record_in = Some(crate::session_store::BackgroundRecord {
            root,
            session: session.to_owned(),
            store,
        });
        self
    }

    /// Persist each subagent's history into its own session file, named by
    /// a fresh session id under the workspace sessions directory.
    pub fn persist_sessions(mut self, root: std::path::PathBuf) -> Self {
        self.persist_root = Some(root);
        self
    }

    /// Set the session storage backend configuration for subagent
    /// persistence. JSONL by default. Each subagent connects its own store
    /// from this config when it starts.
    pub fn with_persist_store(mut self, backend: SessionBackend) -> Self {
        self.persist_backend = backend;
        self
    }

    /// Live session handles (background mode only), for attach views.
    pub fn sessions(&self) -> Sessions {
        self.sessions.clone()
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_runner(
        model: ConfiguredModel,
        context_window: Option<u64>,
        workspace: Workspace,
        background: BackgroundTasks,
        task: DelegatedTask,
        persist: PersistConfig,
        resume_entries: Option<Vec<crate::agent::SessionEntry>>,
    ) -> Result<(SessionHandle, crate::runner::SessionTask), String> {
        let model_name = model.display_name().to_owned();
        let agents_instructions = workspace
            .read_to_string("AGENTS.md")
            .ok()
            .filter(|content| !content.trim().is_empty());
        let tools = crate::tools::builtins_with_background(
            workspace,
            background,
            task.sandbox,
            task.read_only,
        );
        let mut agent = Agent::new(Box::new(model), tools);
        if let Some(window) = context_window {
            agent.set_context_window(window);
        }
        let mut instructions = match task.role_prompt {
            Some(template) => format!(
                "{template}\n\nYou are running as a subagent inside the e-agent coding assistant (on the `{model_name}` model). Work autonomously on the delegated task with the file/bash tools and, when configured, public web search, then return a concise final answer."
            ),
            None => format!(
                "You are a subagent inside the e-agent coding assistant (running on the `{model_name}` model). Work autonomously on the delegated task with the file/bash tools and, when configured, public web search, then return a concise final answer."
            ),
        };
        if task.read_only {
            instructions.push_str(
                "\n\nThis role is read-only: no write_file/edit_file; bash, when present, runs in a read-only sandbox with network disabled.",
            );
        }
        if let Some(content) = agents_instructions {
            instructions.push_str("\n\n## AGENTS.md\n\n");
            instructions.push_str(&content);
        }
        agent.set_context_prefix(instructions);
        if let Some(entries) = resume_entries {
            agent.restore_history(entries);
        }
        let store = SessionStore::connect(&persist.backend, &persist.root, &persist.session_id)
            .await
            .map_err(|e| format!("subagent failed: {e:#}"))?;
        let (runner, handle) = SessionRunner::new(
            agent,
            store,
            persist.root,
            persist.session_id,
            IdlePolicy::FinishWhenIdle,
        );
        let runner_task = runner.start(Some(task.task));
        Ok((handle, runner_task))
    }

    async fn runner_result(
        handle: &SessionHandle,
        task: crate::runner::SessionTask,
    ) -> SessionResult {
        if let Err(error) = task.join().await {
            return SessionResult::Failed(error.to_string());
        }
        match handle.status().borrow().clone() {
            crate::runner::SessionStatus::Finished(result) => result,
            _ => SessionResult::Closed,
        }
    }
}

fn result_output(result: SessionResult) -> (bool, String) {
    match result {
        SessionResult::Completed(answer) => (true, answer.unwrap_or_default()),
        SessionResult::Failed(error) => (false, format!("subagent failed: {error}")),
        SessionResult::Cancelled => (false, "subagent cancelled".into()),
        SessionResult::Closed => (false, "subagent closed".into()),
    }
}

/// Spawn the fire-and-forget creation of a subagent's sessions-metadata
/// row (R3: written by the parent process at spawn time). The parent task
/// id is only known inside the background spawn's synchronous `on_id`
/// hook, so the (async) create is spawned onto the current runtime from
/// there. Best-effort: a failure only logs and never fails the delegate.
fn spawn_subagent_meta_create(
    store: SessionStore,
    root: std::path::PathBuf,
    session_id: String,
    model: String,
    role: Option<&str>,
    parent_session_id: Option<&str>,
    parent_task_id: u64,
) {
    let role = role.map(str::to_owned);
    let parent_session_id = parent_session_id.map(str::to_owned);
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(async move {
                if let Err(error) = store
                    .create_meta(
                        &root,
                        &session_id,
                        Some(&model),
                        role.as_deref(),
                        parent_session_id.as_deref(),
                        Some(parent_task_id as i64),
                    )
                    .await
                {
                    eprintln!("e-agent: cannot record subagent session metadata: {error:#}");
                }
            });
        }
        Err(_) => {
            eprintln!("e-agent: cannot record subagent session metadata: no tokio runtime");
        }
    }
}

fn sync_result(session_id: &str, result: SessionResult) -> Result<String, String> {
    let (completed, output) = result_output(result);
    let output = format!("subagent session: {session_id}\n{output}");
    if completed { Ok(output) } else { Err(output) }
}

#[async_trait]
impl Tool for Delegate {
    fn spec(&self) -> ToolSpec {
        let mut description =
            "Spawn a subagent with a fresh context to work on a task. Use this for \
                self-contained subtasks (searching, reading many files, focused edits) whose \
                intermediate steps would clutter your own context. The subagent has the file and \
                bash tools and, when configured, public web search, but cannot delegate further."
                .to_owned();
        let model = self.subagent_model.display_name();
        description.push_str(&format!(" The subagent runs on the `{model}` model."));
        description.push_str(
            " By default it runs in the background without blocking; the answer arrives \
                automatically as a `[background task N completed]` message. Pass \
                `background: false` to wait for and return the final answer directly. Do not \
                poll, sleep, or re-check a background task — just dispatch it and wait; the \
                completion arrives on its own.",
        );
        if self.persist_root.is_some() {
            description.push_str(
                " With `resume: \"<session-id>\"` the subagent continues a previous \
                subagent session: its transcript is loaded as the starting context and \
                new turns append to the same session file, instead of starting fresh.",
            );
        }
        let roles = self
            .roles_root
            .as_deref()
            .map(crate::roles::available_roles)
            .unwrap_or_default();
        if !roles.is_empty() {
            description.push_str(&format!(
                " Pass `role` to give the subagent a specialized persona/model; available roles: {}. Pass `label` to set a short (≤ 40 chars) title for the task panel; defaults to the role name or a preview of the task.",
                roles.join(", ")
            ));
        }
        let role_property = if roles.is_empty() {
            json!({"type": "string", "description": "specialized role for the subagent"})
        } else {
            json!({"type": "string", "enum": roles, "description": "specialized role for the subagent"})
        };
        ToolSpec {
            name: "delegate".into(),
            description,
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "complete, self-contained instructions for the subagent"},
                    "role": role_property,
                    "label": {"type": "string", "description": "short (≤ 40 chars) human-readable title for the task panel; defaults to the role name or a preview of the task"},
                    "background": {"type": "boolean", "default": true, "description": "run without blocking and deliver the answer as a background completion (default true); pass false to wait for the final answer"},
                    "resume": {"type": "string", "description": "id of a previous subagent session (sub-…) to continue from; its transcript becomes the starting context"},
                    "workspace": {"type": "string", "description": "working directory for the subagent (absolute path); defaults to the parent's workspace"}
                },
                "required": ["task"]
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<String, String> {
        const KNOWN: &[&str] = &["task", "role", "label", "background", "resume", "workspace"];
        if let Some(args) = arguments.as_object() {
            for key in args.keys() {
                if !KNOWN.contains(&key.as_str()) {
                    return Err(format!(
                        "unknown delegate parameter `{key}` (known: {})",
                        KNOWN.join(", ")
                    ));
                }
            }
        }
        let task = arguments
            .as_object()
            .and_then(|args| args.get("task"))
            .and_then(Value::as_str)
            .ok_or("`task` must be a string")?
            .to_owned();
        if task.trim().is_empty() {
            return Err("`task` must not be empty".into());
        }
        let background = match arguments
            .as_object()
            .and_then(|args| args.get("background"))
        {
            None => true,
            Some(value) => value.as_bool().ok_or("`background` must be a boolean")?,
        };
        if background && !self.background.completion_delivery_available() {
            return Err("background task delivery is unavailable".into());
        }
        let role = arguments
            .as_object()
            .and_then(|args| args.get("role"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        // Resolve the role: its model ([roles] <role> > subagent > main), its
        // prompt template and read_only declaration
        // (.e-agent/agents/<role>.md frontmatter). An unknown role is
        // rejected unless no roles are configured at all; a malformed
        // frontmatter is an error (fail closed — the role is not spawned).
        let (model, context_window, role_prompt, read_only) = match role.as_deref() {
            Some(role) => {
                let root = self
                    .roles_root
                    .as_deref()
                    .ok_or("roles are not configured (no workspace roles root)")?;
                let template = crate::roles::role_template(root, role)
                    .map_err(|error| format!("cannot read role `{role}`: {error}"))?
                    .ok_or_else(|| {
                        let available = crate::roles::available_roles(root);
                        format!(
                            "unknown role `{role}` (available: {})",
                            if available.is_empty() {
                                "none".into()
                            } else {
                                available.join(", ")
                            }
                        )
                    })?;
                let model = self
                    .role_models
                    .get(role)
                    .cloned()
                    .unwrap_or_else(|| self.subagent_model.clone());
                let cw = self
                    .role_context_windows
                    .get(role)
                    .copied()
                    .flatten()
                    .or(self.subagent_context_window);
                (model, cw, Some(template.prompt), template.read_only)
            }
            None => (
                self.subagent_model.clone(),
                self.subagent_context_window,
                None,
                false,
            ),
        };
        let resume = arguments
            .as_object()
            .and_then(|args| args.get("resume"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        // Resolve the session to continue, if any: load its transcript (the
        // subagent's starting context) and reuse its id so new turns append
        // to the same file. Without persistence configured there is nothing
        // to resume from.
        let resume = match resume {
            Some(id) => {
                let root = self
                    .persist_root
                    .clone()
                    .ok_or("`resume` requires subagent session persistence (disabled in tests)")?;
                // Connect a temporary store bound to the resumed session id
                // so we load from the correct rows, not the main session's.
                let temp_store = SessionStore::connect(&self.persist_backend, &root, &id)
                    .await
                    .map_err(|error| format!("cannot resume session `{id}`: {error:#}"))?;
                let mut entries = temp_store
                    .load(&root, &id)
                    .await
                    .map_err(|error| format!("cannot resume session `{id}`: {error:#}"))?
                    .entries;
                if entries.is_empty() {
                    return Err(format!("no such subagent session: `{id}`"));
                }
                // Consume any background-task records left by a process that
                // died while this subagent's background delegates were
                // running, so the resumed subagent knows what died with it.
                // Greptime: the running_tasks table is global, so the lookup
                // works across parent sessions. JSONL: records live only in
                // the parent session's file (never a per-subagent file), so
                // there is nothing to consume.
                let unfinished = temp_store
                    .take_unfinished_background_for_subagent(&root, &id)
                    .await
                    .map_err(|error| {
                        format!("cannot load unfinished tasks for session `{id}`: {error:#}")
                    })?;
                if !unfinished.is_empty() {
                    let notice = format!(
                        "[e-agent exited with {} background task(s) still running; they were killed with the process. Re-run them if still needed:]\n{}",
                        unfinished.len(),
                        unfinished.join("\n")
                    );
                    let entry = crate::agent::SessionEntry::Notice {
                        text: notice.clone(),
                    };
                    // Persist immediately (mirrors the main-agent resume
                    // path) so a crash-before-first-turn cannot inject the
                    // same notice again on the next resume.
                    temp_store
                        .append(&root, &id, std::slice::from_ref(&entry))
                        .await
                        .map_err(|error| {
                            format!("cannot persist resume notice for session `{id}`: {error:#}")
                        })?;
                    // Append (NOT restore_history, which would wipe the
                    // resumed history).
                    entries.push(entry);
                }
                Some((id, entries))
            }
            None => None,
        };
        // Parse optional label; fallback chain: label → role → task preview.
        let raw_label = arguments
            .as_object()
            .and_then(|args| args.get("label"))
            .and_then(Value::as_str);
        let label = task_label(raw_label, role.as_deref(), &task);

        // Resolve custom workspace, if given; otherwise inherit the parent's.
        // Track whether a custom workspace was explicitly provided (for
        // display metadata; inherited workspace is not shown).
        let explicit_workspace_arg: Option<String> = arguments
            .as_object()
            .and_then(|args| args.get("workspace"))
            .and_then(Value::as_str)
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        let workspace = match &explicit_workspace_arg {
            Some(path) => self
                .workspace
                .reroot(path)
                .map_err(|error| format!("invalid `workspace` path `{path}`: {error}"))?,
            None => self.workspace.clone(),
        };

        // Build structured display metadata for the F2 task panel. Background
        // reflects the effective execution mode; workspace remains explicit-only.
        let task_display = crate::tools::TaskDisplayMeta {
            background,
            workspace: explicit_workspace_arg.clone(),
        };

        let model_name = model.display_name().to_string();
        let (resume_id, resume_entries) = match resume {
            Some((id, entries)) => (Some(id), Some(entries)),
            None => (None, None),
        };
        let persist = PersistConfig {
            root: self
                .persist_root
                .clone()
                .unwrap_or_else(|| std::env::temp_dir().join("e-agent-subagents")),
            session_id: resume_id.unwrap_or_else(|| crate::session::new_id_prefixed("sub-")),
            backend: self.persist_backend.clone(),
        };
        let session_id = persist.session_id.clone();
        // Sessions metadata (audit table): the subagent's row is written
        // by the PARENT at spawn time — model/role/parent links are all
        // known here (R3) — never by the subagent's own touch path (which
        // read-on-miss caches and rewrites, but never self-creates). The
        // parent task id is allocated by BackgroundTasks only inside the
        // spawn's on_id hook, so the (async) create is spawned from there.
        let meta_store = SessionStore::connect(&persist.backend, &persist.root, &session_id)
            .await
            .map_err(|error| format!("subagent failed: {error:#}"))?;
        let meta_model = model_name.clone();
        let meta_role = role.clone();
        let parent_session_id = self.record_in.as_ref().map(|record| record.session.clone());
        let persist_root = persist.root.clone();
        // Read-only roles run under a narrowed bwrap policy: the workspace is
        // read-only, extra writable roots are dropped, network is disabled.
        let task_sandbox = if read_only {
            self.read_only_sandbox()
        } else {
            self.sandbox.clone()
        };
        let (handle, runner_task) = Self::start_runner(
            model,
            context_window,
            workspace.clone(),
            self.background.clone(),
            DelegatedTask {
                task,
                role_prompt,
                read_only,
                sandbox: task_sandbox,
            },
            persist,
            resume_entries,
        )
        .await?;
        let sessions = self.sessions.clone();
        let slot = Arc::new(Mutex::new(None::<u64>));
        let slot_in_hook = slot.clone();
        let entry = Arc::new(SessionEntry {
            handle: handle.clone(),
            model: model_name,
            role: role.clone(),
            cwd: workspace.root().display().to_string(),
            session_id: session_id.clone(),
            context_window,
        });
        let entry_for_hook = entry.clone();
        // Owned clone for the move closures; the outer `session_id` stays
        // usable for the return messages after the spawn.
        let hook_session_id = session_id.clone();
        if background {
            let record = self.record_in.clone();
            let cleanup = DelegateCleanup::new(slot, self.sessions.clone(), record.clone());
            let record_label = label.clone();
            let record_session_id = session_id.clone();
            let output_session_id = session_id.clone();
            let started = self.background.spawn_with_id(
                label,
                role,
                None,
                Some(task_display),
                move |id| {
                    match slot_in_hook.lock() {
                        Ok(mut slot) => *slot = Some(id),
                        Err(poisoned) => *poisoned.into_inner() = Some(id),
                    }
                    sessions.insert(id, entry_for_hook);
                    if let Some(record) = &record {
                        record.store.record_background_start(
                            &record.root,
                            &record.session,
                            id,
                            &record_label,
                            Some(&record_session_id),
                        );
                    }
                    spawn_subagent_meta_create(
                        meta_store.clone(),
                        persist_root.clone(),
                        hook_session_id.clone(),
                        meta_model.clone(),
                        meta_role.as_deref(),
                        parent_session_id.as_deref(),
                        id,
                    );
                },
                move || {
                    let cleanup = cleanup;
                    async move {
                        let (_, output) =
                            result_output(Self::runner_result(&handle, runner_task).await);
                        cleanup.finish();
                        format!("subagent session: {output_session_id}\n{output}")
                    }
                },
            )?;
            return Ok(format!("{started}\nsubagent session: {session_id}"));
        }
        let cleanup = DelegateCleanup::new(slot, self.sessions.clone(), None);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        self.background.spawn_silent(
            label,
            role,
            None,
            Some(task_display),
            move |id| {
                match slot_in_hook.lock() {
                    Ok(mut slot) => *slot = Some(id),
                    Err(poisoned) => *poisoned.into_inner() = Some(id),
                }
                sessions.insert(id, entry_for_hook);
                spawn_subagent_meta_create(
                    meta_store.clone(),
                    persist_root.clone(),
                    hook_session_id.clone(),
                    meta_model.clone(),
                    meta_role.as_deref(),
                    parent_session_id.as_deref(),
                    id,
                );
            },
            move || {
                let cleanup = cleanup;
                async move {
                    let result = Self::runner_result(&handle, runner_task).await;
                    cleanup.finish();
                    let _ = done_tx.send(result.clone());
                    result_output(result).1
                }
            },
        )?;
        let result = done_rx
            .await
            .map_err(|_| "subagent result channel closed".to_owned())?;
        sync_result(&session_id, result)
    }

    fn set_event_sender(&mut self, sender: tokio::sync::mpsc::UnboundedSender<AgentEvent>) {
        self.background.set_event_sender(sender);
    }
}

#[cfg(test)]
#[path = "delegate_tests.rs"]
mod tests;
