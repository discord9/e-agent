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
    recovery: Option<(std::path::PathBuf, String)>,
}

impl DelegateCleanup {
    fn new(
        id: Arc<Mutex<Option<u64>>>,
        sessions: Sessions,
        recovery: Option<(std::path::PathBuf, String)>,
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
        if let Some((root, session)) = &self.recovery {
            crate::session::Session::clear_background_task(root, session, id);
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
    /// stay visible together and deliver completions through the parent.
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
    /// Parent session's background-task record (root + session name), so
    /// subagent delegates are recorded alongside bash background tasks and
    /// trigger the "killed on exit" notice on restart.
    record_in: Option<(std::path::PathBuf, String)>,
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

    /// Record subagent background tasks in the parent session's record so a
    /// restart can warn about killed subagents alongside killed bash tasks.
    pub fn record_background_tasks_in(mut self, root: std::path::PathBuf, session: &str) -> Self {
        self.record_in = Some((root, session.to_owned()));
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
        let tools = crate::tools::builtins_with_background(workspace, background, task.sandbox);
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

fn sync_result(session_id: &str, result: SessionResult) -> Result<String, String> {
    let (completed, output) = result_output(result);
    let output = format!("subagent session: {session_id}\n{output}");
    if completed { Ok(output) } else { Err(output) }
}

#[async_trait]
impl Tool for Delegate {
    fn spec(&self) -> ToolSpec {
        let mut description =
            "Spawn a subagent with a fresh context to work on a task and return its \
                final answer. Use this for self-contained subtasks (searching, reading many files, \
                focused edits) whose intermediate steps would clutter your own context. The \
                subagent has the file and bash tools and, when configured, public web search, but cannot delegate further."
                .to_owned();
        let model = self.subagent_model.display_name();
        description.push_str(&format!(" The subagent runs on the `{model}` model."));
        description.push_str(
            " With `background: true` it runs without blocking; the answer arrives \
                automatically as a `[background task N completed]` message. Do not poll, \
                sleep, or re-check a background task — just dispatch it and wait; the \
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
                    "background": {"type": "boolean", "description": "run without blocking; the answer arrives as a background completion (default false)"},
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
        let background = arguments
            .as_object()
            .and_then(|args| args.get("background"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if background && !self.background.completion_delivery_available() {
            return Err("background task delivery is unavailable".into());
        }
        let role = arguments
            .as_object()
            .and_then(|args| args.get("role"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        // Resolve the role: its model ([roles] <role> > subagent > main) and
        // its prompt template (.e-agent/agents/<role>.md). An unknown role is
        // rejected unless no roles are configured at all.
        let (model, context_window, role_prompt) = match role.as_deref() {
            Some(role) => {
                let root = self
                    .roles_root
                    .as_deref()
                    .ok_or("roles are not configured (no workspace roles root)")?;
                let prompt = crate::roles::role_prompt(root, role)
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
                (model, cw, Some(prompt))
            }
            None => (
                self.subagent_model.clone(),
                self.subagent_context_window,
                None,
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
                let loaded = temp_store
                    .load(&root, &id)
                    .await
                    .map_err(|error| format!("cannot resume session `{id}`: {error:#}"))?;
                if loaded.entries.is_empty() {
                    return Err(format!("no such subagent session: `{id}`"));
                }
                Some((id, loaded.entries))
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
            Some(path) => Workspace::new(path)
                .map_err(|error| format!("invalid `workspace` path `{path}`: {error}"))?,
            None => self.workspace.clone(),
        };

        // Build structured display metadata for the F2 task panel.
        // Only tracks explicitly-provided values; inherited defaults are not shown.
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
        let (handle, runner_task) = Self::start_runner(
            model,
            context_window,
            workspace.clone(),
            self.background.clone(),
            DelegatedTask {
                task,
                role_prompt,
                sandbox: self.sandbox.clone(),
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
                    if let Some((root, session)) = &record {
                        let _ = crate::session::Session::record_background_start(
                            root,
                            session,
                            id,
                            &record_label,
                            Some(&record_session_id),
                        );
                    }
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
mod tests {
    use super::*;
    use crate::agent::{AssistantMessage, Message, Model, ModelDeltaKind, Usage};
    use crate::tools::builtins;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Notify;

    #[test]
    fn task_label_falls_back_label_then_role_then_task() {
        assert_eq!(
            task_label(Some("  fix the typo  "), Some("fixer"), "a long task"),
            "fix the typo",
            "caller label wins, trimmed"
        );
        assert_eq!(
            task_label(Some("   "), Some("fixer"), "a long task"),
            "fixer",
            "blank label falls through to the role"
        );
        assert_eq!(
            task_label(None, Some("fixer"), "a long task"),
            "fixer",
            "no label falls back to the role"
        );
        assert_eq!(
            task_label(None, None, "a long task"),
            "a long task",
            "no label and no role previews the task"
        );
        assert_eq!(
            task_label(Some("first line\nsecond\tline"), None, "task"),
            "first line second line",
            "labels are normalized onto one started-task line"
        );
        assert_eq!(
            task_label(None, None, "first line\nsecond line"),
            "first line second line",
            "task-preview labels are normalized too"
        );
        let long = "x".repeat(200);
        let capped = task_label(Some(&long), None, "task");
        assert!(
            capped.chars().count() <= 61 && capped.contains('\u{2026}'),
            "caller label is capped with a middle ellipsis, got: {capped:?}"
        );
    }

    #[test]
    fn sync_cancelled_is_an_error_with_session_id() {
        let error = sync_result("sub-cancelled", SessionResult::Cancelled).unwrap_err();
        assert_eq!(
            error, "subagent session: sub-cancelled\nsubagent cancelled",
            "cancelled must never be formatted as a successful sync answer"
        );
    }

    #[test]
    fn registry_tracks_live_sessions() {
        let sessions = Sessions::default();
        assert!(sessions.get(1).is_none());
        let workspace = tempfile::tempdir().unwrap();
        let model = ConfiguredModel::chat(
            crate::model::OpenAiModel::new(
                "http://localhost".into(),
                "test-key".into(),
                "test-model".into(),
                None,
            )
            .unwrap(),
        );
        let agent = Agent::new(Box::new(model), vec![]);
        let (_runner, handle) = SessionRunner::new(
            agent,
            SessionStore::Jsonl,
            workspace.path().to_path_buf(),
            "sub-test".into(),
            IdlePolicy::FinishWhenIdle,
        );
        let entry = Arc::new(SessionEntry {
            handle,
            model: "test-model".into(),
            role: None,
            cwd: "/tmp".into(),
            session_id: "sub-test".into(),
            context_window: None,
        });
        sessions.insert(1, entry);
        assert!(sessions.get(1).is_some());
        sessions.remove(1);
        assert!(sessions.get(1).is_none());
    }

    fn delegate_with_url(workspace: &std::path::Path, base_url: String) -> Delegate {
        let workspace = Workspace::new(workspace).unwrap();
        // Construct the model with a dummy key directly: tests must not
        // depend on (or mutate) process env.
        let model = ConfiguredModel::chat(
            crate::model::OpenAiModel::new(base_url, "test-key".into(), "test-model".into(), None)
                .unwrap(),
        );
        let (_, background) = builtins(workspace.clone(), None);
        Delegate::new(model, workspace, background)
    }

    fn delegate(workspace: &std::path::Path) -> Delegate {
        delegate_with_url(workspace, "http://localhost".into())
    }

    struct ProbeModel {
        entered: Arc<Notify>,
        release: Arc<Notify>,
        future_dropped: Arc<Notify>,
        model_dropped: Arc<Notify>,
        side_effects: Arc<AtomicUsize>,
        panic: bool,
    }

    struct FutureDropProbe(Arc<Notify>);

    impl Drop for FutureDropProbe {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }

    impl Drop for ProbeModel {
        fn drop(&mut self) {
            self.model_dropped.notify_one();
        }
    }

    #[async_trait]
    impl Model for ProbeModel {
        async fn complete(
            &mut self,
            _: &[Message],
            _: &[ToolSpec],
            _: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
        ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
            let _probe = FutureDropProbe(self.future_dropped.clone());
            self.entered.notify_one();
            assert!(!self.panic, "controlled model panic");
            self.release.notified().await;
            self.side_effects.fetch_add(1, Ordering::SeqCst);
            Ok((
                AssistantMessage {
                    content: Some("finished".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
                None,
            ))
        }
    }

    struct ProbeSignals {
        entered: Arc<Notify>,
        release: Arc<Notify>,
        future_dropped: Arc<Notify>,
        model_dropped: Arc<Notify>,
        side_effects: Arc<AtomicUsize>,
    }

    fn probe_runner(
        root: &std::path::Path,
        panic: bool,
    ) -> (SessionHandle, crate::runner::SessionTask, ProbeSignals) {
        let signals = ProbeSignals {
            entered: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            future_dropped: Arc::new(Notify::new()),
            model_dropped: Arc::new(Notify::new()),
            side_effects: Arc::new(AtomicUsize::new(0)),
        };
        let agent = Agent::new(
            Box::new(ProbeModel {
                entered: signals.entered.clone(),
                release: signals.release.clone(),
                future_dropped: signals.future_dropped.clone(),
                model_dropped: signals.model_dropped.clone(),
                side_effects: signals.side_effects.clone(),
                panic,
            }),
            vec![],
        );
        let (runner, handle) = SessionRunner::new(
            agent,
            SessionStore::Jsonl,
            root.to_path_buf(),
            "probe".into(),
            IdlePolicy::FinishWhenIdle,
        );
        (handle, runner.start(Some("start".into())), signals)
    }

    fn probe_entry(handle: SessionHandle) -> Arc<SessionEntry> {
        Arc::new(SessionEntry {
            handle,
            model: "probe".into(),
            role: None,
            cwd: "/tmp".into(),
            session_id: "sub-probe".into(),
            context_window: None,
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn background_cancel_during_on_id_cleans_registration_without_completion() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let workspace = Workspace::new(&root).unwrap();
        let (_, mut background) = builtins(workspace, None);
        let (sender, mut completions) = tokio::sync::mpsc::unbounded_channel();
        background.set_event_sender(sender);
        let (handle, runner_task, signals) = probe_runner(&root, false);
        let sessions = Sessions::default();
        let slot = Arc::new(Mutex::new(None));
        let cleanup = DelegateCleanup::new(
            slot.clone(),
            sessions.clone(),
            Some((root.clone(), "parent".into())),
        );
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let work_runs = Arc::new(AtomicUsize::new(0));
        let spawn_background = background.clone();
        let hook_slot = slot.clone();
        let hook_sessions = sessions.clone();
        let hook_handle = handle.clone();
        let hook_root = root.clone();
        let hook_entered = entered.clone();
        let hook_release = release.clone();
        let spawn_work_runs = work_runs.clone();

        let spawn = tokio::task::spawn_blocking(move || {
            spawn_background.spawn_with_id(
                "probe".into(),
                None,
                None,
                None,
                move |id| {
                    *hook_slot.lock().unwrap() = Some(id);
                    hook_sessions.insert(id, probe_entry(hook_handle));
                    crate::session::Session::record_background_start(
                        &hook_root,
                        "parent",
                        id,
                        "probe",
                        Some("sub-probe"),
                    )
                    .unwrap();
                    hook_entered.wait();
                    hook_release.wait();
                },
                move || {
                    spawn_work_runs.fetch_add(1, Ordering::SeqCst);
                    let cleanup = cleanup;
                    async move {
                        let result = Delegate::runner_result(&handle, runner_task).await;
                        cleanup.finish();
                        result_output(result).1
                    }
                },
            )
        });

        entered.wait();
        assert!(sessions.get(1).is_some());
        assert_eq!(background.cancel(1).as_deref(), Some("probe"));
        release.wait();
        assert!(spawn.await.unwrap().is_ok(), "spawn must not panic or fail");
        signals.model_dropped.notified().await;
        signals.release.notify_one();
        tokio::task::yield_now().await;
        assert!(background.running().is_empty());
        assert!(sessions.sessions.lock().unwrap().is_empty());
        assert!(crate::session::Session::take_unfinished_background(&root, "parent").is_empty());
        assert_eq!(work_runs.load(Ordering::SeqCst), 0);
        assert_eq!(signals.side_effects.load(Ordering::SeqCst), 0);
        assert!(completions.try_recv().is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn background_cancel_before_first_yield_cleans_everything() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(temp.path()).unwrap();
        let (_, mut background) = builtins(workspace, None);
        let (sender, mut completions) = tokio::sync::mpsc::unbounded_channel();
        background.set_event_sender(sender);
        let (handle, runner_task, signals) = probe_runner(temp.path(), false);
        let sessions = Sessions::default();
        let slot = Arc::new(Mutex::new(None));
        let cleanup = DelegateCleanup::new(
            slot.clone(),
            sessions.clone(),
            Some((temp.path().to_path_buf(), "parent".into())),
        );
        let hook_slot = slot.clone();
        let hook_sessions = sessions.clone();
        let hook_handle = handle.clone();
        let hook_root = temp.path().to_path_buf();
        background
            .spawn_with_id(
                "probe".into(),
                None,
                None,
                None,
                move |id| {
                    *hook_slot.lock().unwrap() = Some(id);
                    hook_sessions.insert(id, probe_entry(hook_handle.clone()));
                    crate::session::Session::record_background_start(
                        &hook_root,
                        "parent",
                        id,
                        "probe",
                        Some("sub-probe"),
                    )
                    .unwrap();
                },
                move || {
                    let cleanup = cleanup;
                    async move {
                        let result = Delegate::runner_result(&handle, runner_task).await;
                        cleanup.finish();
                        result_output(result).1
                    }
                },
            )
            .unwrap();

        assert_eq!(background.cancel(1).as_deref(), Some("probe"));
        signals.model_dropped.notified().await;
        signals.release.notify_one();
        tokio::task::yield_now().await;
        assert!(background.running().is_empty());
        assert!(sessions.sessions.lock().unwrap().is_empty());
        assert!(
            crate::session::Session::take_unfinished_background(temp.path(), "parent").is_empty()
        );
        assert_eq!(signals.side_effects.load(Ordering::SeqCst), 0);
        assert!(completions.try_recv().is_err());
    }

    #[tokio::test]
    async fn background_cancel_while_joining_aborts_inner_without_completion() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(temp.path()).unwrap();
        let (_, mut background) = builtins(workspace, None);
        let (sender, mut completions) = tokio::sync::mpsc::unbounded_channel();
        background.set_event_sender(sender);
        let (handle, runner_task, signals) = probe_runner(temp.path(), false);
        let sessions = Sessions::default();
        let slot = Arc::new(Mutex::new(None));
        let cleanup = DelegateCleanup::new(
            slot.clone(),
            sessions.clone(),
            Some((temp.path().to_path_buf(), "parent".into())),
        );
        let hook_sessions = sessions.clone();
        let hook_handle = handle.clone();
        let hook_root = temp.path().to_path_buf();
        background
            .spawn_with_id(
                "probe".into(),
                None,
                None,
                None,
                move |id| {
                    *slot.lock().unwrap() = Some(id);
                    hook_sessions.insert(id, probe_entry(hook_handle.clone()));
                    crate::session::Session::record_background_start(
                        &hook_root,
                        "parent",
                        id,
                        "probe",
                        Some("sub-probe"),
                    )
                    .unwrap();
                },
                move || {
                    let cleanup = cleanup;
                    async move {
                        let result = Delegate::runner_result(&handle, runner_task).await;
                        cleanup.finish();
                        result_output(result).1
                    }
                },
            )
            .unwrap();

        signals.entered.notified().await;
        assert_eq!(background.cancel(1).as_deref(), Some("probe"));
        signals.future_dropped.notified().await;
        signals.model_dropped.notified().await;
        signals.release.notify_one();
        tokio::task::yield_now().await;
        assert!(background.running().is_empty());
        assert!(sessions.sessions.lock().unwrap().is_empty());
        assert!(
            crate::session::Session::take_unfinished_background(temp.path(), "parent").is_empty()
        );
        assert_eq!(signals.side_effects.load(Ordering::SeqCst), 0);
        assert!(completions.try_recv().is_err());
    }

    #[tokio::test]
    async fn sync_cancel_cleans_session_and_closes_result_channel() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(temp.path()).unwrap();
        let (_, mut background) = builtins(workspace, None);
        let (sender, mut completions) = tokio::sync::mpsc::unbounded_channel();
        background.set_event_sender(sender);
        let (handle, runner_task, signals) = probe_runner(temp.path(), false);
        let sessions = Sessions::default();
        let slot = Arc::new(Mutex::new(None));
        let cleanup = DelegateCleanup::new(slot.clone(), sessions.clone(), None);
        let hook_sessions = sessions.clone();
        let hook_handle = handle.clone();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        background
            .spawn_silent(
                "probe".into(),
                None,
                None,
                None,
                move |id| {
                    *slot.lock().unwrap() = Some(id);
                    hook_sessions.insert(id, probe_entry(hook_handle.clone()));
                },
                move || {
                    let cleanup = cleanup;
                    async move {
                        let result = Delegate::runner_result(&handle, runner_task).await;
                        cleanup.finish();
                        let _ = done_tx.send(result.clone());
                        result_output(result).1
                    }
                },
            )
            .unwrap();

        signals.entered.notified().await;
        assert_eq!(background.cancel(1).as_deref(), Some("probe"));
        assert_eq!(
            done_rx.await.unwrap_err().to_string(),
            "channel closed",
            "sync delegate reports its existing channel-closed error"
        );
        signals.future_dropped.notified().await;
        signals.model_dropped.notified().await;
        assert!(background.running().is_empty());
        assert!(sessions.sessions.lock().unwrap().is_empty());
        assert!(completions.try_recv().is_err());
    }

    #[tokio::test]
    async fn panicking_inner_model_cleans_up_and_sends_one_failure_completion() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(temp.path()).unwrap();
        let (_, mut background) = builtins(workspace, None);
        let (sender, mut completions) = tokio::sync::mpsc::unbounded_channel();
        background.set_event_sender(sender);
        let (handle, runner_task, signals) = probe_runner(temp.path(), true);
        let sessions = Sessions::default();
        let slot = Arc::new(Mutex::new(None));
        let cleanup = DelegateCleanup::new(slot.clone(), sessions.clone(), None);
        let hook_sessions = sessions.clone();
        let hook_handle = handle.clone();
        background
            .spawn_with_id(
                "probe".into(),
                None,
                None,
                None,
                move |id| {
                    *slot.lock().unwrap() = Some(id);
                    hook_sessions.insert(id, probe_entry(hook_handle.clone()));
                },
                move || {
                    let cleanup = cleanup;
                    async move {
                        let result = Delegate::runner_result(&handle, runner_task).await;
                        cleanup.finish();
                        result_output(result).1
                    }
                },
            )
            .unwrap();

        let event = completions.recv().await.unwrap();
        assert!(matches!(
            event,
            AgentEvent::BackgroundCompleted { output, .. }
                if output.starts_with("subagent failed:") && output.contains("controlled model panic")
        ));
        signals.model_dropped.notified().await;
        assert!(background.running().is_empty());
        assert!(sessions.sessions.lock().unwrap().is_empty());
        assert!(
            completions.try_recv().is_err(),
            "exactly one completion is sent"
        );
    }

    async fn successful_model(answer: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let answer = serde_json::to_string(answer).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let header_end = loop {
                let mut chunk = [0; 1024];
                let count = stream.read(&mut chunk).await.unwrap();
                request.extend_from_slice(&chunk[..count]);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = std::str::from_utf8(&request[..header_end]).unwrap();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            let received_body = request.len() - header_end;
            let mut rest = vec![0; content_length - received_body];
            stream.read_exact(&mut rest).await.unwrap();

            let body = format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":{answer}}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn rejects_empty_task() {
        let temp = tempfile::tempdir().unwrap();
        let delegate = delegate(temp.path());
        assert!(
            delegate
                .execute(json!({"task": "  "}))
                .await
                .unwrap_err()
                .contains("must not be empty")
        );
        assert!(
            delegate
                .execute(json!({}))
                .await
                .unwrap_err()
                .contains("must be a string")
        );
    }

    #[tokio::test]
    async fn background_delivery_preflight_fails_before_later_work() {
        // The missing sender must win over resume loading and workspace
        // construction, and must not allocate or persist anything.
        let temp = tempfile::tempdir().unwrap();
        let persist_root = temp.path().join("subagent-sessions");
        let record_root = temp.path().join("parent-session");
        let delegate = delegate(temp.path())
            .persist_sessions(persist_root.clone())
            .record_background_tasks_in(record_root.clone(), "parent");

        let error = delegate
            .execute(json!({
                "task": "hi",
                "background": true,
                "resume": "sub-does-not-exist",
                "workspace": "/nonexistent-path-that-surely-does-not-exist-12345"
            }))
            .await
            .unwrap_err();

        assert_eq!(error, "background task delivery is unavailable");
        assert!(delegate.background.running().is_empty());
        assert!(delegate.sessions.sessions.lock().unwrap().is_empty());
        assert!(!persist_root.exists(), "resume store must not connect");
        assert!(
            !record_root.exists(),
            "background record must not be written"
        );
    }

    #[tokio::test]
    async fn resume_requires_persistence_and_an_existing_session() {
        // No persistence configured: nothing to resume from.
        let temp = tempfile::tempdir().unwrap();
        let no_persist = delegate(temp.path());
        assert!(
            no_persist
                .execute(json!({"task": "hi", "resume": "sub-x"}))
                .await
                .unwrap_err()
                .contains("requires subagent session persistence")
        );

        // Persistence configured but the session id does not exist.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        let tool: Delegate = delegate(temp.path()).persist_sessions(root);
        assert!(
            tool.execute(json!({"task": "hi", "resume": "sub-does-not-exist"}))
                .await
                .unwrap_err()
                .contains("no such subagent session")
        );
    }

    #[test]
    fn resume_loads_the_previous_transcript_as_starting_context() {
        // The core resume invariant: a persisted sub- session can be loaded
        // back, and its length marks where new-turn appends begin (so the
        // loaded history is NOT re-persisted).
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        let prior = vec![
            crate::agent::SessionEntry::from(crate::agent::Message::User {
                content: "earlier task".into(),
            }),
            crate::agent::SessionEntry::from(crate::agent::Message::Assistant(
                crate::agent::AssistantMessage {
                    content: Some("earlier answer".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
            )),
        ];
        crate::session::Session::append(&root, "sub-prior", &prior).unwrap();

        let loaded = crate::session::Session::load(&root, "sub-prior").unwrap();
        assert_eq!(loaded.entries.len(), prior.len());
        // persisted_len starts at the loaded length, so the next append only
        // writes entries from index `loaded.len()` onward.
        let new_entries = &prior[loaded.entries.len()..];
        assert!(new_entries.is_empty(), "loaded history is not re-persisted");
    }

    #[tokio::test]
    async fn spec_disallows_nested_delegation_by_design() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(temp.path()).unwrap();
        let (tools, _) = builtins(workspace, None);
        let names: Vec<String> = tools.iter().map(|tool| tool.spec().name).collect();
        assert!(!names.contains(&"delegate".to_owned()));
        assert!(names.contains(&"bash".to_owned()));
    }

    #[tokio::test]
    // Test-only env isolation: the std Mutex guard is held across .execute()
    // awaits to serialize XDG_CONFIG_HOME mutation with roles.rs tests. The
    // critical sections are short and never contend in practice.
    #[allow(clippy::await_holding_lock)]
    async fn role_requires_a_roles_root_and_a_known_role() {
        let _guard = crate::roles::XDG_TEST_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        // Isolate from the developer's real global agents directory.
        let xdg = temp.path().join("xdg-empty");
        std::fs::create_dir_all(&xdg).unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };

        // No roles root: any role is rejected.
        let plain = delegate(temp.path());
        assert!(
            plain
                .execute(json!({"task": "hi", "role": "fixer"}))
                .await
                .unwrap_err()
                .contains("roles are not configured")
        );

        // Roles root set, but the requested role has no template file.
        let rooted = delegate(temp.path()).with_roles_root(temp.path().to_path_buf());
        let error = rooted
            .execute(json!({"task": "hi", "role": "fixer"}))
            .await
            .unwrap_err();
        assert!(error.contains("unknown role `fixer`"), "{error}");

        // A template on disk (workspace `agents/`) makes the role valid.
        let directory = temp.path().join("agents");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("fixer.md"), "You fix things.").unwrap();
        let spec = rooted.spec();
        let roles = spec.parameters["properties"]["role"]["enum"]
            .as_array()
            .unwrap();
        assert_eq!(roles, &vec![json!("fixer")]);

        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    }

    #[tokio::test]
    async fn shared_background_bash_completion_reaches_the_parent_channel() {
        // A bash tool bound to the parent's BackgroundTasks keeps that
        // sender when wrapped in an Agent (Agent::new must not retarget
        // it): a background command's completion arrives on the parent's
        // channel even after the subagent is dropped. End-to-end subagent
        // behaviour is covered by agent.rs's shared-sender test; here we
        // pin the runner wiring.
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(temp.path()).unwrap();
        let (_, mut parent_background) = builtins(workspace.clone(), None);
        let (parent_sender, mut parent_receiver) =
            tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        parent_background.set_event_sender(parent_sender);

        let started = parent_background
            .start(workspace, "echo shared".into(), false)
            .unwrap();
        assert!(started.starts_with("started background task"));

        let event = tokio::time::timeout(std::time::Duration::from_secs(5), parent_receiver.recv())
            .await
            .expect("parent channel got the completion")
            .unwrap();
        assert!(matches!(
            event,
            AgentEvent::BackgroundCompleted { output, .. } if output.contains("shared")
        ));
    }

    #[tokio::test]
    async fn delegate_uses_custom_workspace() {
        let parent = tempfile::tempdir().unwrap();
        // Create a separate tempdir as the custom workspace.
        let custom = tempfile::tempdir().unwrap();
        // Put a marker file in each workspace.
        std::fs::write(custom.path().join("sentinel.txt"), "custom").unwrap();
        std::fs::write(parent.path().join("sentinel.txt"), "parent").unwrap();

        // 1) An invalid (non-existent) workspace path is rejected at
        //    parameter-validation time, before any subagent is spawned.
        let tool = delegate(parent.path());
        let err = tool
            .execute(json!({
                "task": "irrelevant",
                "workspace": "/nonexistent-path-that-surely-does-not-exist-12345"
            }))
            .await
            .unwrap_err();
        assert!(
            err.contains("invalid `workspace`"),
            "expected invalid-workspace error, got: {err}"
        );

        // 2) A valid custom workspace is accepted; the subagent tries to
        //    contact the dummy model (localhost) and fails with a connection
        //    error — but crucially the workspace error is NOT raised.
        let tool = delegate(parent.path());
        let answer = tool
            .execute(json!({
                "task": "read sentinel.txt and report its content",
                "workspace": custom.path().to_str().unwrap()
            }))
            .await
            .unwrap_err();
        assert!(
            answer.contains("\nsubagent failed:"),
            "expected model-connection failure, got: {answer}"
        );
        assert!(
            answer.starts_with("subagent session: sub-"),
            "sync failure must identify its subagent session, got: {answer}"
        );
        assert!(
            !answer.contains("invalid `workspace`"),
            "valid workspace path should not produce a workspace error, got: {answer}"
        );
    }

    #[tokio::test]
    async fn sync_success_contains_session_id_and_answer() {
        let temp = tempfile::tempdir().unwrap();
        let base_url = successful_model("finished answer").await;
        let tool = delegate_with_url(temp.path(), base_url);

        let output = tool.execute(json!({"task": "hello"})).await.unwrap();
        let mut lines = output.lines();
        let session_id = lines
            .next()
            .and_then(|line| line.strip_prefix("subagent session: "))
            .expect("sync success contains the subagent session id");
        assert!(session_id.starts_with("sub-"));
        assert_eq!(lines.collect::<Vec<_>>(), ["finished answer"]);
    }

    #[tokio::test]
    async fn background_delegate_completion_contains_session_id() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");
        let base_url = successful_model("finished answer").await;
        let mut tool = delegate_with_url(temp.path(), base_url).persist_sessions(root);
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        tool.set_event_sender(sender);

        let answer = tool
            .execute(json!({
                "task": "hello",
                "label": "first line\nsecond line",
                "background": true
            }))
            .await
            .unwrap();
        let mut lines = answer.lines();
        assert_eq!(
            lines.next(),
            Some("started background task 1: first line second line")
        );
        let immediate_session = lines
            .next()
            .and_then(|line| line.strip_prefix("subagent session: "))
            .expect("immediate result contains the subagent session id");
        assert!(immediate_session.starts_with("sub-"));
        assert_eq!(lines.next(), None);

        let event = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
            .await
            .expect("timed out waiting for background completion")
            .unwrap();
        assert!(tool.background.running().is_empty());
        assert!(tool.sessions.sessions.lock().unwrap().is_empty());
        assert!(
            receiver.try_recv().is_err(),
            "exactly one completion is sent"
        );
        match event {
            AgentEvent::BackgroundCompleted { output, .. } => {
                assert_eq!(
                    output,
                    format!("subagent session: {immediate_session}\nfinished answer"),
                    "successful completion must retain the immediate session id and answer"
                );
            }
            other => panic!("expected BackgroundCompleted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn background_failure_completion_retains_session_id() {
        let temp = tempfile::tempdir().unwrap();
        let mut tool = delegate(temp.path());
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        tool.set_event_sender(sender);

        let answer = tool
            .execute(json!({"task": "hello", "background": true}))
            .await
            .unwrap();
        let immediate_session = answer
            .lines()
            .nth(1)
            .and_then(|line| line.strip_prefix("subagent session: "))
            .expect("immediate result contains the subagent session id");

        let event = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
            .await
            .expect("timed out waiting for background failure")
            .unwrap();
        match event {
            AgentEvent::BackgroundCompleted { output, .. } => assert!(
                output.starts_with(&format!(
                    "subagent session: {immediate_session}\nsubagent failed:"
                )),
                "failed completion must retain the main-branch session format, got: {output}"
            ),
            other => panic!("expected BackgroundCompleted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resume_replays_scrollback_into_session_sink() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sessions");

        // Create a persisted prior session with two entries.
        let prior = vec![
            crate::agent::SessionEntry::from(crate::agent::Message::User {
                content: "earlier task".into(),
            }),
            crate::agent::SessionEntry::from(crate::agent::Message::Assistant(
                crate::agent::AssistantMessage {
                    content: Some("earlier answer".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
            )),
        ];
        crate::session::Session::append(&root, "sub-resume-scrollback", &prior).unwrap();

        let mut tool = delegate(temp.path()).persist_sessions(root);
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        tool.set_event_sender(sender);

        // Spawn a background delegate that resumes the prior session.
        let answer = tool
            .execute(json!({"task": "new prompt", "background": true, "resume": "sub-resume-scrollback"}))
            .await
            .unwrap();
        assert!(answer.starts_with("started background task"));

        let id: u64 = answer
            .strip_prefix("started background task ")
            .and_then(|s| s.split(':').next())
            .and_then(|s| s.trim().parse().ok())
            .expect("could not extract task id");

        // Give the subagent task a moment to emit the scrollback events.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let entry = tool.sessions().get(id).expect("session entry missing");
        let snapshot = entry.handle.snapshot();

        // The snapshot should contain the prior UserPrompt before the new one.
        let user_texts: Vec<&str> = snapshot
            .iter()
            .filter_map(|e| match e {
                AgentEvent::UserPrompt(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            user_texts.len() >= 2,
            "expected at least two UserPrompt events (prior + new), got {user_texts:?}"
        );
        assert_eq!(user_texts[0], "earlier task");
        // The new prompt may or may not be last (replayed prior events
        // come first); just check it appears.
        assert!(
            user_texts.contains(&"new prompt"),
            "expected 'new prompt' in UserPrompt events, got {user_texts:?}"
        );

        // The prior AssistantText must be in the log too.
        let assistant_texts: Vec<&str> = snapshot
            .iter()
            .filter_map(|e| match e {
                AgentEvent::AssistantText(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            assistant_texts.contains(&"earlier answer"),
            "expected 'earlier answer' in AssistantText events, got {assistant_texts:?}"
        );
    }
}
