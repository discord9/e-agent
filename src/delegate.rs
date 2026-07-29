//! `delegate` tool: spawn a subagent with a fresh context to work on a task.
//!
//! Each subagent runs on its own OS thread with its own current-thread
//! tokio runtime, its own background-task slots, and an empty history. This
//! keeps all subagent state (history, pending background results, token
//! counts) fully isolated from the parent agent.
//!
//! The subagent gets the builtin file/bash tools (no MCP tools, no `delegate`
//! itself — depth is capped at 1 by construction). In background mode the
//! answer is delivered as a [`AgentEvent::BackgroundCompleted`] through the
//! parent's event channel, waking an idle agent (see Slice 1).
//!
//! Every background subagent is exposed through a [`LiveSession`] handle
//! (see `handle.rs`): frontends attach to it for a full live view and can
//! steer it (queue prompts / cancel the in-flight turn). Sync delegates
//! stay single-turn and handle-less.
//!
//! Future evolution: the thread boundary is deliberately the same shape as a
//! process boundary — swapping `std::thread::spawn` for a spawned
//! `e-agent --subagent` subprocess with a stdio JSONL protocol is the
//! planned path to stronger isolation. MCP tools can be added later by
//! letting the subagent run `mcp::connect_all` inside its own runtime.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::{Agent, AgentEvent, Tool, ToolSpec, preview};
use crate::handle::{SessionHandle, SessionSink, SessionSource, Steer, session_channel};
use crate::model::OpenAiModel;
use crate::session::Session;
use crate::tools::{BackgroundTasks, builtins};
use crate::workspace::Workspace;

/// Maximum rounds a subagent may take before giving up. Aligned with the
/// main agent's MAX_TOOL_ROUNDS.
const SUBAGENT_MAX_ROUNDS: usize = 32;
/// Ceiling for a synchronous delegate call.
const SYNC_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Registry of live session handles, keyed by background-task id (the same
/// unified id sequence shared with background bash). Background subagents
/// register their handle here so the TUI can attach; entries are removed
/// when the subagent finishes.
#[derive(Clone, Default)]
pub struct Sessions {
    sessions: Arc<Mutex<std::collections::HashMap<u64, Arc<dyn SessionHandle>>>>,
}

impl Sessions {
    pub fn get(&self, id: u64) -> Option<Arc<dyn SessionHandle>> {
        self.sessions.lock().unwrap().get(&id).cloned()
    }

    pub fn insert(&self, id: u64, handle: Arc<dyn SessionHandle>) {
        self.sessions.lock().unwrap().insert(id, handle);
    }

    pub fn remove(&self, id: u64) {
        self.sessions.lock().unwrap().remove(&id);
    }
}

/// Pick the next prompt for a steerable subagent: stashed prompts first
/// (arrival order), then whatever is queued in the channel, then block until
/// a prompt arrives or the channel closes (None = shut down).
/// Take the next already-queued prompt (stashed mid-turn), if any, without
/// blocking. Returns None when nothing is queued — the caller treats that
/// as "subagent done", because the session handle keeps the steer channel
/// open until the work returns, so blocking here would never see `None`.
fn next_queued_prompt(source: &mut SessionSource, pending: &mut Vec<String>) -> Option<String> {
    if !pending.is_empty() {
        return Some(pending.remove(0));
    }
    while let Some(message) = source.try_recv() {
        if let Steer::Prompt(text) = message {
            return Some(text);
        }
    }
    None
}

/// Append the history entries produced since the last call to the
/// subagent's own session file (`subagent-<task-id>`). Every subagent has
/// its own file, so no marker or locking is needed. Best-effort: a
/// persistence failure is logged, not fatal.
fn persist_turn(persist: &PersistConfig, agent: &Agent, persisted: &mut usize) {
    let new_entries = &agent.history()[*persisted..];
    if new_entries.is_empty() {
        return;
    }
    if let Err(error) = Session::append(&persist.root, &persist.session_id, new_entries) {
        eprintln!("e-agent: cannot persist subagent transcript: {error:#}");
        return;
    }
    *persisted = agent.history().len();
}

pub struct Delegate {
    model: OpenAiModel,
    workspace: Workspace,
    /// Shared background slots (with bash): a background delegate occupies
    /// one slot and its completion is delivered as a background completion.
    background: BackgroundTasks,
    /// Live handles of background subagents, for the TUI attach view.
    sessions: Sessions,
    /// Directory where each subagent persists its own session file
    /// (None = in-memory only, e.g. tests). Files are named after a fresh
    /// unique session id (`sub-<timestamp>-<rand>`), never the background
    /// task id — task ids restart at 1 every process and would collide
    /// across restarts.
    persist_root: Option<std::path::PathBuf>,
}

/// Where a subagent writes its own session file.
#[derive(Clone)]
pub struct PersistConfig {
    root: std::path::PathBuf,
    /// This subagent's session id, assigned at spawn time.
    session_id: String,
}

impl Delegate {
    pub fn new(model: OpenAiModel, workspace: Workspace, background: BackgroundTasks) -> Self {
        Self {
            model,
            workspace,
            background,
            sessions: Sessions::default(),
            persist_root: None,
        }
    }

    /// Persist each subagent's history into its own session file, named by
    /// a fresh session id under the workspace sessions directory.
    pub fn persist_sessions(mut self, root: std::path::PathBuf) -> Self {
        self.persist_root = Some(root);
        self
    }

    /// Live session handles (background mode only), for attach views.
    pub fn sessions(&self) -> Sessions {
        self.sessions.clone()
    }

    /// Run `task` on a dedicated thread with a fresh agent and return the
    /// final answer. Used by both sync and background execution so the two
    /// modes share one code path. With a `steering` pair the subagent's
    /// events are mirrored into the session (frontends rebuild their view
    /// from snapshot + stream) and it accepts steering — queued prompts
    /// become fresh turns, cancel drops the in-flight turn. Sync mode
    /// passes None and keeps the original single-turn behaviour.
    fn run_on_thread(
        model: OpenAiModel,
        workspace: Workspace,
        task: String,
        steering: Option<(SessionSink, SessionSource)>,
        persist: Option<PersistConfig>,
    ) -> String {
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => return format!("cannot build subagent runtime: {error}"),
            };
            runtime.block_on(async move {
                let (tools, _) = builtins(workspace);
                let mut agent =
                    Agent::new(Box::new(model), tools).max_tool_rounds(SUBAGENT_MAX_ROUNDS);
                // A bare single-user-message request is rejected by some
                // providers (kimi k3 answers HTTP 403 to `msgs=1`); give the
                // subagent a minimal system prompt so its first call always
                // carries a system + user pair.
                agent.set_context_prefix(
                    "You are a subagent inside the e-agent coding assistant. Work \
                     autonomously on the delegated task with the file/bash tools, \
                     then return a concise final answer."
                        .into(),
                );
                let (sink, mut source) = match steering {
                    Some((sink, source)) => {
                        agent.observe(sink.clone());
                        (Some(sink), Some(source))
                    }
                    None => (None, None),
                };
                // Prompts stashed while a turn was running, in arrival order.
                let mut pending: Vec<String> = Vec::new();
                let mut prompt = task;
                let mut last_answer = String::new();
                // History entries already persisted (per-turn incremental).
                let mut persisted_len = 0usize;
                // Record the delegated task in the session log so an attached
                // view shows what the subagent was asked to do, not just the
                // tool calls that follow.
                if let Some(sink) = &sink {
                    sink.emit(AgentEvent::UserPrompt(prompt.clone()));
                }
                loop {
                    let result = {
                        let run = agent.run(prompt);
                        tokio::pin!(run);
                        match source.as_mut() {
                            // Sync mode: no steering, just run to completion.
                            None => run.await,
                            Some(source) => {
                                let mut cancelled = false;
                                let result = loop {
                                    tokio::select! {
                                        result = &mut run => break Some(result),
                                        message = source.recv() => match message {
                                            // Cancel: drop the in-flight turn
                                            // (bash subprocesses are killed via
                                            // their process-group guard on
                                            // drop); completed rounds stay in
                                            // history.
                                            Some(Steer::Cancel) => {
                                                cancelled = true;
                                                break None;
                                            }
                                            // Prompt mid-turn: stash it; the
                                            // post-turn drain picks it up.
                                            Some(Steer::Prompt(text)) => {
                                                pending.push(text);
                                            }
                                            None => break Some(run.await),
                                        },
                                    }
                                };
                                if cancelled {
                                    if let Some(sink) = &sink {
                                        sink.emit(AgentEvent::AssistantText(
                                            "[turn cancelled by user]".into(),
                                        ));
                                    }
                                    Ok(String::new())
                                } else {
                                    result.expect("run completed")
                                }
                            }
                        }
                    };
                    last_answer = match result {
                        Ok(answer) => answer,
                        Err(error) => {
                            // Surface the failure in the session log so an
                            // attached view shows it instead of going blank.
                            let message = format!("subagent failed: {error:#}");
                            if let Some(sink) = &sink {
                                sink.emit(AgentEvent::AssistantText(message.clone()));
                            }
                            message
                        }
                    };
                    // Persist this turn's entries (append-only) so the full
                    // transcript survives restarts; entries are tagged with
                    // the subagent's label for display.
                    if let Some(persist) = &persist {
                        persist_turn(persist, &agent, &mut persisted_len);
                    }
                    // Turn ended. Prompts stashed mid-turn still get their
                    // own follow-up turns; once those are drained the
                    // subagent is done. We must NOT block waiting for new
                    // steer messages here: the session handle is held by the
                    // registry until this work returns, so the steer channel
                    // never closes and waiting would deadlock (the completion
                    // event would never reach the parent).
                    let Some(source) = source.as_mut() else {
                        return last_answer;
                    };
                    // Drain only already-queued prompts (non-blocking).
                    prompt = match next_queued_prompt(source, &mut pending) {
                        Some(text) => text,
                        None => return last_answer,
                    };
                    // The steered prompt is already in the log (send_input
                    // emits UserPrompt), so no extra echo here.
                }
            })
        })
        .join()
        .unwrap_or_else(|_| "subagent thread panicked".into())
    }
}

#[async_trait]
impl Tool for Delegate {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "delegate".into(),
            description: "Spawn a subagent with a fresh context to work on a task and return its \
                final answer. Use this for self-contained subtasks (searching, reading many files, \
                focused edits) whose intermediate steps would clutter your own context. The \
                subagent has the file and bash tools but cannot delegate further. With \
                `background: true` it runs without blocking; the answer arrives as a \
                background task completion."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "complete, self-contained instructions for the subagent"},
                    "background": {"type": "boolean", "description": "run without blocking; the answer arrives as a background completion (default false)"}
                },
                "required": ["task"]
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<String, String> {
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

        if background {
            let model = self.model.clone();
            let workspace = self.workspace.clone();
            let label = preview(&task, 100);
            let (handle, sink, source) = session_channel();
            let session: Arc<dyn SessionHandle> = Arc::new(handle.clone());
            let sessions = self.sessions.clone();
            let sessions_in_work = self.sessions.clone();
            let slot = std::sync::Arc::new(std::sync::Mutex::new(None::<u64>));
            let slot_in_hook = slot.clone();
            let slot_in_work = slot.clone();
            // Fresh unique session id per subagent (never the task id —
            // task ids restart at 1 every process and would collide).
            let persist = self.persist_root.clone().map(|root| PersistConfig {
                root,
                session_id: crate::session::new_id_prefixed("sub-"),
            });
            // run_on_thread blocks on thread::join, so push it onto the
            // blocking thread pool to keep the executor responsive.
            return self.background.spawn_with_id(
                label,
                None,
                move |id| {
                    sessions.insert(id, session);
                    *slot_in_hook.lock().unwrap() = Some(id);
                },
                move || async move {
                    let output = tokio::task::spawn_blocking(move || {
                        Self::run_on_thread(model, workspace, task, Some((sink, source)), persist)
                    })
                    .await
                    .unwrap_or_else(|error| format!("subagent blocking task failed: {error}"));
                    if let Some(id) = *slot_in_work.lock().unwrap() {
                        sessions_in_work.remove(id);
                    }
                    output
                },
            );
        }

        let model = self.model.clone();
        let workspace = self.workspace.clone();
        let persist = self.persist_root.clone().map(|root| PersistConfig {
            root,
            session_id: crate::session::new_id_prefixed("sub-"),
        });
        let handle = tokio::task::spawn_blocking(move || {
            Self::run_on_thread(model, workspace, task, None, persist)
        });
        match tokio::time::timeout(SYNC_TIMEOUT, handle).await {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(error)) => Err(format!("subagent thread failed: {error}")),
            Err(_) => Err("subagent timed out after 30 minutes".into()),
        }
    }

    fn set_event_sender(&mut self, sender: tokio::sync::mpsc::UnboundedSender<AgentEvent>) {
        self.background.set_event_sender(sender);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_tracks_live_sessions() {
        let sessions = Sessions::default();
        assert!(sessions.get(1).is_none());
        let (handle, _sink, _source) = session_channel();
        sessions.insert(1, Arc::new(handle));
        assert!(sessions.get(1).is_some());
        sessions.remove(1);
        assert!(sessions.get(1).is_none());
    }

    fn delegate(workspace: &std::path::Path) -> Delegate {
        let workspace = Workspace::new(workspace).unwrap();
        let model = OpenAiModel::from_env(None, None).unwrap();
        let (_, background) = builtins(workspace.clone());
        Delegate::new(model, workspace, background)
    }

    #[tokio::test]
    async fn rejects_empty_task() {
        unsafe { std::env::set_var("OPENAI_API_KEY", "test-key") };
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
    async fn background_fails_without_event_sender() {
        // BackgroundTasks without set_event_sender cannot deliver results.
        let temp = tempfile::tempdir().unwrap();
        let delegate = delegate(temp.path());
        assert!(
            delegate
                .execute(json!({"task": "hi", "background": true}))
                .await
                .unwrap_err()
                .contains("delivery is unavailable")
        );
    }

    #[tokio::test]
    async fn spec_disallows_nested_delegation_by_design() {
        unsafe { std::env::set_var("OPENAI_API_KEY", "test-key") };
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(temp.path()).unwrap();
        let (tools, _) = builtins(workspace);
        let names: Vec<String> = tools.iter().map(|tool| tool.spec().name).collect();
        assert!(!names.contains(&"delegate".to_owned()));
        assert!(names.contains(&"bash".to_owned()));
    }
}
