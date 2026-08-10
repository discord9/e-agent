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
    /// The subagent's own session store, already bound to `session_id` at
    /// spawn time (both construction sites have one connected). The web
    /// server reads a subagent's transcript through this store without
    /// re-connecting per request; `handle` alone cannot serve history
    /// (the runner's event log is a recent tail, not the full transcript).
    pub store: SessionStore,
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

    /// Snapshot of every `(task_id, entry)` pair; the caller may hold the
    /// entries beyond the lock. Used by the web server's subagent lookup
    /// (address a subagent by its session id, not its task id) and by the
    /// TUI attach panel.
    pub fn list(&self) -> Vec<(u64, Arc<SessionEntry>)> {
        self.sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(id, entry)| (*id, entry.clone()))
            .collect()
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

/// Live role-model resolver installed by the session factory: resolves a
/// role's model from the reloadable config at spawn time (returns the model
/// plus its context window, or `None` when the role is not routed).
pub type RoleModelSource =
    Arc<dyn Fn(&str) -> Option<(ConfiguredModel, Option<u64>)> + Send + Sync>;

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
    /// Live role-model resolver installed by the session factory: resolves
    /// a role's model from the reloadable config at spawn time, so `[roles]`
    /// edits hot-reload into newly spawned subagents without a restart.
    /// `None` (a Delegate constructed directly, e.g. in tests) falls back to
    /// the construction-time `role_models` snapshot.
    role_model_source: Option<RoleModelSource>,
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
    /// Upper bound for a `FinishWhenIdle` subagent's wait on its blocking
    /// background tasks (`None` = wait indefinitely). Resolved from
    /// `[delegate] finalize_wait_secs` by the session factory.
    finalize_wait: Option<std::time::Duration>,
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
        .map(|label| preview(&single_line(label), 40))
        .or_else(|| role.map(single_line))
        .unwrap_or_else(|| preview(&single_line(task), 40))
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
    /// True (the default, incl. roles without a `protect_git` frontmatter
    /// key) when the subagent's bash must bind `<workspace>/.git` read-only
    /// on non-Windows. A role with `protect_git = false` opts out — required
    /// for any bash at all under the Windows write-sandbox MVP, which cannot
    /// enforce the protection and fails closed instead.
    protect_git: bool,
    sandbox: Option<crate::config::Sandbox>,
    /// True for a btw fork: an interactive side conversation, not a one-shot
    /// delegated task. Swaps the system instructions (no "return a concise
    /// final answer" — the user keeps replying) and pairs with a
    /// `WaitForInput` idle policy so the subagent stays alive.
    interactive: bool,
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
            role_model_source: None,
            roles_root: None,
            sandbox: None,
            record_in: None,
            persist_backend: SessionBackend::default(),
            finalize_wait: None,
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

    /// Resolve role models live at spawn time (from the session factory's
    /// reloadable config) instead of the construction-time snapshot. The
    /// source returns `(model, context window)` for a role, or `None` when
    /// the role is not routed — the `role_models` snapshot then applies.
    pub fn with_role_model_source(mut self, source: RoleModelSource) -> Self {
        self.role_model_source = Some(source);
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

    /// Cap a `FinishWhenIdle` subagent's wait on its blocking background
    /// tasks (see `[delegate] finalize_wait_secs`); `None` waits
    /// indefinitely. The subagent finalizes on expiry without cancelling
    /// the tasks, which keep running in the shared registry.
    pub fn with_finalize_wait(mut self, wait: Option<std::time::Duration>) -> Self {
        self.finalize_wait = wait;
        self
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
        policy: IdlePolicy,
        finalize_wait: Option<std::time::Duration>,
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
            task.protect_git,
            // The subagent itself is a delegate task in the parent's shared
            // registry; pass its own session id so get_background_tasks can
            // annotate which entry is "itself".
            Some(persist.session_id.clone()),
        );
        let mut agent = Agent::new(Box::new(model), tools);
        if let Some(window) = context_window {
            agent.set_context_window(window);
        }
        let mut instructions = match task.role_prompt {
            Some(template) => format!(
                "{template}\n\nYou are running as a subagent inside the e-agent coding assistant (on the `{model_name}` model). Work autonomously on the delegated task with the file/bash tools and, when configured, public web search, then return a concise final answer."
            ),
            // btw fork: an interactive side conversation forked from the
            // main session. No "final answer" framing — the subagent runs
            // under WaitForInput and the user keeps replying to it.
            None if task.interactive => format!(
                "You are a persistent subagent in a btw fork of the main e-agent session (running on the `{model_name}` model). This is an interactive side conversation that continues the main session's history outside the main line: discuss the user's question with the file/bash tools and, when configured, public web search, and keep the conversation going — the user may keep replying, so do not treat a single answer as final."
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
        // 后台 bash 任务完成时会自动以 `[background task N completed]` 消息注入
        // 到本会话（普通 delegate 与 btw fork 的 runner 都走这条路径），无需轮询：
        // dispatch 后台任务后继续其它工作或直接等待即可，不要反复调用
        // get_background_tasks / sleep 重查。
        // 若你 dispatch 了后台任务且其结果属于最终答案的一部分：在后台任务完成之前
        // 不要给出最终答案收尾（FinishWhenIdle 会等待它完成并注入结果触发新 turn）；
        // 收到注入后必须把结果整合进完整最终答案，不要只简短确认「完成了」。
        instructions.push_str(
            "\n\n后台任务（background bash）完成时会自动以 `[background task N completed]` 消息注入，无需轮询：dispatch 后台任务后继续其它工作或直接等待即可，不要反复调用 get_background_tasks / sleep 重查。若你 dispatch 了后台任务且其结果属于最终答案的一部分：在后台任务完成之前不要给出最终答案收尾（FinishWhenIdle 会等待它完成并自动注入结果触发新 turn）；收到 `[background task N completed]` 注入后，把结果整合进你的完整最终答案，不要只简短确认「完成了」——你的任务是给出含后台任务结果的完整答案。",
        );
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
        // 把 subagent 的后台 bash 任务记到它自己的 session 名下：after_tool_entry
        // 借此登记 subagent 的 bash 任务（此前 background_record 为 None，subagent
        // 的 bash 任务既不进 running_tasks 重启后丢失，面板也定位不到发起者）。
        // store 是 subagent 自己的（persist 绑定 session_id），root 同 workspace；
        // take_unfinished_background 按 session 消费（服务器僵尸扫描对子会话行组
        // 单独消费），与主会话路径兼容。
        agent.record_background_tasks_in(persist.root.clone(), &persist.session_id, store.clone());
        let (runner, handle) =
            SessionRunner::new(agent, store, persist.root, persist.session_id, policy);
        let runner = runner.with_finalize_wait(finalize_wait);
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
#[allow(clippy::too_many_arguments)]
fn spawn_subagent_meta_create(
    store: SessionStore,
    root: std::path::PathBuf,
    session_id: String,
    model: String,
    role: Option<&str>,
    parent_session_id: Option<&str>,
    parent_task_id: u64,
    label: Option<String>,
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
                        label.as_deref(),
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

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// btw fork subagent (`/btw <question>` backend)
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

/// Everything [`spawn_btw_subagent`] needs to fork a live session's history
/// into a persistent interactive subagent: the model/tools configuration the
/// subagent inherits from its parent session, plus the registries it joins.
/// The server endpoint (and, in the future, the TUI `/btw` wiring)
/// assembles this from the parent session's live state + the session
/// factory.
pub struct BtwContext {
    /// The subagent's model: the parent session's own model.
    pub model: ConfiguredModel,
    /// Context window for `model` (the parent's).
    pub context_window: Option<u64>,
    /// Workspace the subagent works in (the parent's).
    pub workspace: Workspace,
    /// bwrap policy inherited from the parent (`None` = sandbox disabled).
    pub sandbox: Option<crate::config::Sandbox>,
    /// Read-only policy inherited from the parent session.
    pub read_only: bool,
    /// Shared running-task registry: the btw task-panel entry and the
    /// subagent's own background bash both live here.
    pub background: BackgroundTasks,
    /// Live subagent registry the btw subagent registers into (TUI attach).
    pub sessions: Sessions,
    /// Directory where the subagent persists its session file.
    pub persist_root: std::path::PathBuf,
    /// Session backend configuration for the subagent's store.
    pub backend: SessionBackend,
    /// Parent session's background record (root + parent session id + the
    /// parent's store): records the btw task for the killed-on-exit notice
    /// and supplies the `parent_session_id` metadata link.
    pub record_in: Option<crate::session_store::BackgroundRecord>,
}

/// The initial history of a btw fork: the source session's prefix up to its
/// last completed turn (the same cut [`crate::agent::fork_prefix`] applies
/// to `--fork` main sessions), then the `ForkedFrom` marker, then the
/// explanatory notice line. The question itself is NOT part of the history
/// — it is delivered as the runner's initial prompt (the first user
/// message).
fn btw_fork_entries(
    source: &str,
    entries: &[crate::agent::SessionEntry],
) -> Result<Vec<crate::agent::SessionEntry>, String> {
    let prefix = crate::agent::fork_prefix(entries, None)?;
    let at = prefix.len();
    let mut fork_entries = Vec::with_capacity(prefix.len() + 2);
    fork_entries.extend(prefix);
    fork_entries.push(crate::agent::SessionEntry::ForkedFrom {
        source: source.to_owned(),
        at,
        // JSONL has no event_time column and the fork path keeps seq as an
        // optional provenance slot (mirrors session_factory's fork marker).
        event_time: None,
        seq: None,
    });
    fork_entries.push(crate::agent::SessionEntry::Notice {
        text: "（btw fork：在主线之外继续探讨）".to_owned(),
    });
    Ok(fork_entries)
}

/// Fork a live session's full history into a persistent interactive
/// "btw fork" subagent and start it with `question` as its first user
/// message. Returns the new subagent's session id (e.g. `btw-…`).
///
/// Semantics (user-confirmed):
/// - The source session's history is loaded and cut at the last completed
///   turn, stamped with a `ForkedFrom` marker plus the
///   「（btw fork：在主线之外继续探讨）」 notice, and written to a fresh
///   `btw-…` subagent session — the same fork rule `--fork` main sessions
///   use, but as the SUBAGENT's starting history (the `resume_entries`
///   path of [`Delegate::start_runner`]).
/// - The subagent runs under [`IdlePolicy::WaitForInput`] and stays
///   registered for the whole conversation. Unlike a `delegate` tool task
///   it is NOT a one-shot delegated task: there is no parent tool loop
///   waiting for an answer, so it never "completes" and is never cleaned up
///   as a finished delegate. Its task-panel entry and `Sessions` entry live
///   until the user cancels the task (or the process exits — the parent's
///   background record then reports it as killed on the next launch).
/// - `question` is queued as the runner's initial prompt, so the first turn
///   starts immediately; the main session is untouched and continues.
pub async fn spawn_btw_subagent(
    source_session: &str,
    question: &str,
    context: BtwContext,
) -> Result<String, String> {
    let question = question.trim();
    if question.is_empty() {
        return Err("prompt must not be empty".into());
    }
    let BtwContext {
        model,
        context_window,
        workspace,
        sandbox,
        read_only,
        background,
        sessions,
        persist_root,
        backend,
        record_in,
    } = context;
    let model_name = model.display_name().to_owned();
    let cwd = workspace.root().display().to_string();
    // Load the source session's full history; the fork source is only read,
    // exactly like the `--fork` path in session_factory.
    let source_store = SessionStore::connect(&backend, &persist_root, source_session)
        .await
        .map_err(|e| format!("btw fork failed: {e:#}"))?;
    let source_entries = source_store
        .load(&persist_root, source_session)
        .await
        .map_err(|e| format!("btw fork failed: {e:#}"))?
        .entries;
    let fork_entries = btw_fork_entries(source_session, &source_entries)
        .map_err(|e| format!("cannot fork session {source_session}: {e}"))?;
    let session_id = crate::session::new_id_prefixed("btw-");
    let persist = PersistConfig {
        root: persist_root.clone(),
        session_id: session_id.clone(),
        backend: backend.clone(),
    };
    // Persist the fork entries into the fresh subagent session BEFORE the
    // runner starts: the runner only appends new entries, restored history
    // is never written back (the delegate `resume` path relies on the
    // entries already living in the file). JSONL rewrite / Greptime append
    // mirror the main-session fork path.
    let fork_store = SessionStore::connect(&backend, &persist.root, &session_id)
        .await
        .map_err(|e| format!("btw fork failed: {e:#}"))?;
    match &backend {
        SessionBackend::Jsonl => fork_store
            .rewrite(&persist.root, &session_id, &fork_entries)
            .await
            .map_err(|e| format!("btw fork failed: {e:#}"))?,
        // Greptime: fresh session (no rows, next_seq = 0); append writes
        // contiguous seqs with fresh timestamps (the fork marker's
        // provenance fields are payload-only).
        SessionBackend::Greptime { .. } => fork_store
            .append(&persist.root, &session_id, &fork_entries)
            .await
            .map_err(|e| format!("btw fork failed: {e:#}"))?,
        // SQLite: same append-only semantics as Greptime (a fresh session
        // has no rows, so append writes contiguous seqs from 0).
        SessionBackend::Sqlite { .. } => fork_store
            .append(&persist.root, &session_id, &fork_entries)
            .await
            .map_err(|e| format!("btw fork failed: {e:#}"))?,
    }
    let (handle, runner_task) = Delegate::start_runner(
        model,
        context_window,
        workspace,
        background.clone(),
        DelegatedTask {
            task: question.to_owned(),
            role_prompt: None,
            read_only,
            // btw fork has no role frontmatter: keep the historical
            // subagent default (protect .git).
            protect_git: true,
            sandbox,
            interactive: true,
        },
        persist,
        Some(fork_entries),
        IdlePolicy::WaitForInput,
        // A btw fork never finalizes on its own (WaitForInput), so the
        // finalize wait does not apply; keep the runner's default.
        None,
    )
    .await?;
    // Sessions metadata: the subagent's row links back to the parent
    // (parent_session_id = source session) and its btw task id, exactly
    // like a delegate's row (R3: written by the parent at spawn time).
    // Connected here (before `persist` is consumed) with the same root.
    let meta_store = SessionStore::connect(&backend, &persist_root, &session_id)
        .await
        .map_err(|e| format!("btw fork failed: {e:#}"))?;
    let parent_session_id = record_in.as_ref().map(|record| record.session.clone());
    let meta_persist_root = persist_root.clone();
    // Register as a persistent task-panel entry. The panel entry is what
    // makes the subagent attachable (the TUI F2 panel and the web task
    // panel key off the shared task registry, and the `Sessions` entry
    // holds the attach handle). The wrapper's work future never completes
    // on its own — with WaitForInput the runner only ends when the user
    // cancels the task (or the process exits) — so this registration lives
    // for the whole conversation instead of being cleaned up like a
    // finished delegate task. `DelegateCleanup` still runs on cancel so a
    // cancelled btw session does not leak its registration or background
    // record.
    let label = format!("btw: {}", task_label(None, None, question));
    let sessions_hook = sessions.clone();
    let slot = Arc::new(Mutex::new(None::<u64>));
    let slot_in_hook = slot.clone();
    let entry = Arc::new(SessionEntry {
        handle: handle.clone(),
        model: model_name.clone(),
        role: None,
        cwd,
        session_id: session_id.clone(),
        context_window,
        // Bound to this subagent's session id (connected just above with
        // `persist_root` + `session_id`); lets the web server read the
        // btw transcript without re-connecting per request.
        store: meta_store.clone(),
    });
    let entry_for_hook = entry.clone();
    let hook_session_id = session_id.clone();
    let cleanup = DelegateCleanup::new(slot, sessions.clone(), record_in.clone());
    let record = record_in.clone();
    let record_label = label.clone();
    let meta_label = label.clone();
    let record_session_id = session_id.clone();
    let output_session_id = session_id.clone();
    background.spawn_with_id(
        label,
        None,
        None,
        Some(crate::tools::TaskDisplayMeta {
            background: true,
            workspace: None,
            subagent_session_id: Some(session_id.clone()),
            resume: None,
        }),
        move |id| {
            match slot_in_hook.lock() {
                Ok(mut slot) => *slot = Some(id),
                Err(poisoned) => *poisoned.into_inner() = Some(id),
            }
            sessions_hook.insert(id, entry_for_hook);
            if let Some(record) = &record {
                record.store.record_background_start(
                    &record.root,
                    &record.session,
                    id,
                    &record_label,
                    None,
                    Some(&record_session_id),
                );
            }
            spawn_subagent_meta_create(
                meta_store.clone(),
                meta_persist_root.clone(),
                hook_session_id.clone(),
                model_name.clone(),
                None,
                parent_session_id.as_deref(),
                id,
                Some(meta_label),
            );
        },
        move || {
            let cleanup = cleanup;
            async move {
                let (_, output) =
                    result_output(Delegate::runner_result(&handle, runner_task).await);
                cleanup.finish();
                format!("btw session: {output_session_id}\n{output}")
            }
        },
    )?;
    Ok(session_id)
}

#[async_trait]
impl Tool for Delegate {
    fn spec(&self) -> ToolSpec {
        let mut description =
            "The first argument MUST be workspace: emit its path (absolute, or relative to \
                your own workspace root — e.g. `.e-agent/worktrees/wt-x`) before writing task. \
                Spawn a subagent with a fresh context to work on a task. Use this for \
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
                    "workspace": {"type": "string", "description": "REQUIRED — working directory for the subagent (e.g. /home/user/project or C:\\Users\\user\\project). May be absolute or relative: relative paths are resolved against YOUR workspace root (e.g. `.e-agent/worktrees/wt-x`) and must stay inside it or an authorized external directory."},
                    "role": role_property,
                    "label": {"type": "string", "description": "short (≤ 40 chars) human-readable title for the task panel; defaults to the role name or a preview of the task"},
                    "background": {"type": "boolean", "default": true, "description": "run without blocking and deliver the answer as a background completion (default true); pass false to wait for the final answer"},
                    "resume": {"type": "string", "description": "id of a previous subagent session (sub-…) to continue from; its transcript becomes the starting context"},
                    "task": {"type": "string", "description": "complete, self-contained instructions for the subagent"},
                },
                "required": ["workspace", "task"]
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
        // prompt template, read_only and protect_git declarations
        // (.e-agent/agents/<role>.md frontmatter). An unknown role is
        // rejected unless no roles are configured at all; a malformed
        // frontmatter is an error (fail closed — the role is not spawned).
        let (model, context_window, role_prompt, read_only, protect_git) = match role.as_deref() {
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
                let (model, cw) = match &self.role_model_source {
                    // Live source (session factory): pick up hot-reloaded
                    // `[roles]` routing; when the role is not routed, fall
                    // back to the default subagent model like the snapshot
                    // path below.
                    Some(source) => source(role).unwrap_or_else(|| {
                        (self.subagent_model.clone(), self.subagent_context_window)
                    }),
                    // Construction-time snapshot (direct Delegate use/tests).
                    None => (
                        self.role_models
                            .get(role)
                            .cloned()
                            .unwrap_or_else(|| self.subagent_model.clone()),
                        self.role_context_windows
                            .get(role)
                            .copied()
                            .flatten()
                            .or(self.subagent_context_window),
                    ),
                };
                (
                    model,
                    cw,
                    Some(template.prompt),
                    template.read_only,
                    template.protect_git,
                )
            }
            None => (
                self.subagent_model.clone(),
                self.subagent_context_window,
                None,
                false,
                // No role: keep the historical subagent default (protect
                // .git, exactly like the fixer path).
                true,
            ),
        };
        let resume = arguments
            .as_object()
            .and_then(|args| args.get("resume"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        // Raw resume id for display metadata: the resolution match below
        // moves the string, so keep a copy before it is consumed.
        let resume_display = resume.clone();
        // Resolve the session to continue, if any: load its transcript (the
        // subagent's starting context) and reuse its id so new turns append
        // to the same file. Without persistence configured there is nothing
        // to resume from.
        let resume = match resume {
            Some(id) => {
                // Resume guard (concurrent-write conflicts #46/#49): never
                // continue a subagent whose runner is STILL live in this
                // parent's `Sessions` registry — the resumed runner would
                // append to the same session file concurrently. A handle
                // exists only while the subagent is alive (DelegateCleanup
                // removes finished ones), so a hit here always means "still
                // running"; the subagent continues on its own and needs no
                // resume. Cross-process rows are deliberately NOT consulted
                // here: this path is the zombie-row recovery mechanism
                // (`take_unfinished_background_for_subagent` below consumes
                // them), so blocking on a surviving row would deadlock it.
                if self
                    .sessions
                    .list()
                    .iter()
                    .any(|(_, entry)| entry.session_id == id)
                {
                    return Err(format!(
                        "cannot resume session `{id}`: it is still running as a live subagent; \
                         wait for it to finish or cancel its task first"
                    ));
                }
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

        // Resolve the (required) workspace: the subagent's working directory
        // is an explicit parameter — there is no parent-workspace fallback.
        // Absolute paths are used as-is; relative paths are resolved against
        // the CALLER's workspace root before reroot() canonicalizes and
        // enforces authorization (the resolved path must stay inside the
        // caller's workspace or an authorized writable external directory;
        // reroot() rejects non-absolute paths itself, hence the join here).
        let workspace_arg: Option<String> = arguments
            .as_object()
            .and_then(|args| args.get("workspace"))
            .and_then(Value::as_str)
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        let Some(workspace_arg) = workspace_arg else {
            return Err(
                "delegate requires a workspace parameter: path of the working directory \
                 (absolute, or relative to this workspace's root)"
                    .into(),
            );
        };
        // Join relative inputs with the caller's root up front; reroot's
        // canonicalize then resolves any `.`/`..` segments while refusing
        // escapes out of the authorized tree.
        let workspace_path = if std::path::Path::new(&workspace_arg).is_absolute() {
            std::path::PathBuf::from(&workspace_arg)
        } else {
            self.workspace.root().join(&workspace_arg)
        };
        let workspace = self
            .workspace
            .reroot(&workspace_path)
            .map_err(|error| format!("invalid `workspace` path `{workspace_arg}`: {error}"))?;

        let model_name = model.display_name().to_string();
        let (resume_id, resume_entries) = match resume {
            Some((id, entries)) => (Some(id), Some(entries)),
            None => (None, None),
        };
        // A fresh spawn records the task-panel label as the subagent
        // session's title; a resume must never overwrite the title the
        // original spawn recorded (create_meta on an existing row is a
        // backfill no-op that keeps the existing title).
        let meta_label = if resume_id.is_some() {
            None
        } else {
            Some(label.clone())
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
        // Build structured display metadata for the F2 task panel. Background
        // reflects the effective execution mode; workspace is always explicit
        // (required parameter). The workspace shown is the user's ORIGINAL
        // input (relative stays relative — friendlier in the panel than the
        // resolved absolute path). The subagent session id lets the web task
        // panel jump straight to the subagent's transcript without label
        // matching (labels are lost once the Greptime `running_tasks` row is
        // cleared at completion).
        let task_display = crate::tools::TaskDisplayMeta {
            background,
            workspace: Some(workspace_arg.clone()),
            subagent_session_id: Some(session_id.clone()),
            resume: resume_display,
        };
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
                protect_git,
                sandbox: task_sandbox,
                interactive: false,
            },
            persist,
            resume_entries,
            IdlePolicy::FinishWhenIdle,
            self.finalize_wait,
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
            // Bound to this subagent's session id (connected just above
            // with `persist.root` + `session_id`); lets the web server read
            // the subagent's transcript without re-connecting per request.
            store: meta_store.clone(),
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
                            None,
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
                        meta_label.clone(),
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
        // The sync wrapper races the subagent runner against `done_tx.closed()`
        // (fires when the main side drops `done_rx` — cancel/timeout/leave of
        // the turn that is synchronously awaiting this delegate). On
        // abandonment it aborts the subagent runner instead of leaving it to
        // orphan and keep running (bash, file edits, session writes) until it
        // finishes naturally. The abort output is discarded by `spawn_silent`.
        let cancel_output = format!("subagent session: {session_id}\nsubagent cancelled");
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
                    meta_label.clone(),
                );
            },
            move || {
                let mut cleanup = Some(cleanup);
                let mut done_tx = done_tx;
                async move {
                    let mut runner_task = Some(runner_task);
                    tokio::select! {
                        _ = done_tx.closed() => {
                            // 主侧已放弃（取消/超时/离开）：中止子 runner，立即清理。
                            if let Some(mut task) = runner_task.take() {
                                task.abort();
                            }
                            cleanup.take().expect("cleanup taken once").finish();
                            cancel_output
                        }
                        result = async {
                            let task = runner_task.take().expect("runner task taken once");
                            Self::runner_result(&handle, task).await
                        } => {
                            cleanup.take().expect("cleanup taken once").finish();
                            let _ = done_tx.send(result.clone());
                            result_output(result).1
                        }
                    }
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
