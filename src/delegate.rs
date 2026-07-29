//! `delegate` tool: spawn a subagent with a fresh context to work on a task.
//!
//! Each subagent runs on its own OS thread with its own current-thread
//! tokio runtime and an empty history. Its agent state (history, pending
//! background results, token counts) is isolated from the parent.
//!
//! The subagent gets the builtin file/bash tools and, when configured, public
//! web search (no MCP tools, no `delegate` itself — depth is capped at 1 by
//! construction). In background mode the answer is delivered as a
//! [`AgentEvent::BackgroundCompleted`] through the parent's event channel,
//! waking an idle agent (see Slice 1).
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

use crate::agent::{Agent, AgentEvent, Model, Tool, ToolSpec, preview};
use crate::handle::{SessionHandle, SessionSink, SessionSource, Steer, session_channel};
use crate::model::ConfiguredModel;
use crate::session::Session;
use crate::tools::BackgroundTasks;
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
    /// Subagents run on the role-routed model when configured, otherwise
    /// on the main model.
    subagent_model: ConfiguredModel,
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
}

/// Where a subagent writes its own session file.
#[derive(Clone)]
pub struct PersistConfig {
    root: std::path::PathBuf,
    /// This subagent's session id, assigned at spawn time.
    session_id: String,
}

impl Delegate {
    pub fn new(model: ConfiguredModel, workspace: Workspace, background: BackgroundTasks) -> Self {
        Self {
            subagent_model: model,
            workspace,
            background,
            sessions: Sessions::default(),
            persist_root: None,
        }
    }

    /// Route subagents onto a different model (e.g. a cheaper profile from
    /// `[roles] subagent = "…"`). Without this they share the main model.
    pub fn with_subagent_model(mut self, model: ConfiguredModel) -> Self {
        self.subagent_model = model;
        self
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
    ///
    /// The subagent's bash tool shares the parent's `background` registry,
    /// so a background bash command started by a subagent shows up in the
    /// parent's task panel and delivers its
    /// completion to the PARENT agent — it survives the subagent's end
    /// instead of being silently killed and forgotten.
    fn run_on_thread(
        model: ConfiguredModel,
        workspace: Workspace,
        background: BackgroundTasks,
        task: String,
        steering: Option<(SessionSink, SessionSource)>,
        persist: Option<PersistConfig>,
        resume_entries: Option<Vec<crate::agent::SessionEntry>>,
    ) -> String {
        let role = model.name().to_owned();
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => return format!("cannot build subagent runtime: {error}"),
            };
            runtime.block_on(async move {
                let agents_instructions = workspace
                    .read_to_string("AGENTS.md")
                    .ok()
                    .filter(|content| !content.trim().is_empty());
                let tools = crate::tools::builtins_with_background(workspace, background);
                let mut agent =
                    Agent::new(Box::new(model), tools).max_tool_rounds(SUBAGENT_MAX_ROUNDS);
                // A bare single-user-message request is rejected by some
                // providers (kimi k3 answers HTTP 403 to `msgs=1`); give the
                // subagent a minimal system prompt so its first call always
                // carries a system + user pair.
                let mut instructions = format!(
                    "You are a subagent inside the e-agent coding assistant (running on the \
                     `{role}` model). Work autonomously on the delegated task with the \
                     file/bash tools and, when configured, public web search, then return a concise final answer."
                );
                if let Some(content) = agents_instructions {
                    instructions.push_str("\n\n## AGENTS.md\n\n");
                    instructions.push_str(&content);
                }
                agent.set_context_prefix(instructions);
                let (sink, mut source) = match steering {
                    Some((sink, source)) => {
                        agent.observe(sink.clone());
                        (Some(sink), Some(source))
                    }
                    None => (None, None),
                };
                // Resuming: seed the agent with the previous session's
                // transcript and mark it already persisted, so persist_turn
                // only appends the NEW turns (no duplicate replay of the
                // loaded history).
                let mut persisted_len = 0usize;
                if let Some(entries) = resume_entries {
                    persisted_len = entries.len();
                    agent.restore_history(entries);
                }
                // Prompts stashed while a turn was running, in arrival order.
                let mut pending: Vec<String> = Vec::new();
                let mut prompt = task;
                let mut last_answer = String::new();
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

/// Cancels a synchronous subagent when the future blocked on it is dropped
/// (the parent turn was cancelled). Mirrors [`crate::tools::ProcessGroupGuard`]:
/// armed right after spawn, disarmed on the normal completion path, so only
/// an actual drop-while-blocked fires the cancel.
struct SubagentCancelGuard {
    handle: Option<Arc<dyn SessionHandle>>,
}

impl SubagentCancelGuard {
    fn armed(handle: Arc<dyn SessionHandle>) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    fn disarm(&mut self) {
        self.handle = None;
    }
}

impl Drop for SubagentCancelGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.cancel();
        }
    }
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
        let model = self.subagent_model.name();
        description.push_str(&format!(" The subagent runs on the `{model}` model."));
        description.push_str(
            " With `background: true` it runs without blocking; the answer arrives as a \
                background task completion.",
        );
        if self.persist_root.is_some() {
            description.push_str(
                " With `resume: \"<session-id>\"` the subagent continues a previous \
                subagent session: its transcript is loaded as the starting context and \
                new turns append to the same session file, instead of starting fresh.",
            );
        }
        ToolSpec {
            name: "delegate".into(),
            description,
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "complete, self-contained instructions for the subagent"},
                    "background": {"type": "boolean", "description": "run without blocking; the answer arrives as a background completion (default false)"},
                    "resume": {"type": "string", "description": "id of a previous subagent session (sub-…) to continue from; its transcript becomes the starting context"}
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
                let loaded = crate::session::Session::load(&root, &id)
                    .map_err(|error| format!("cannot resume session `{id}`: {error:#}"))?;
                if loaded.entries.is_empty() {
                    return Err(format!("no such subagent session: `{id}`"));
                }
                Some((id, loaded.entries))
            }
            None => None,
        };

        if background {
            let model = self.subagent_model.clone();
            let workspace = self.workspace.clone();
            let background = self.background.clone();
            let label = preview(&task, 100);
            let (handle, sink, source) = session_channel();
            let session: Arc<dyn SessionHandle> = Arc::new(handle.clone());
            let sessions = self.sessions.clone();
            let sessions_in_work = self.sessions.clone();
            let slot = std::sync::Arc::new(std::sync::Mutex::new(None::<u64>));
            let slot_in_hook = slot.clone();
            let slot_in_work = slot.clone();
            // Fresh unique session id per subagent (never the task id —
            // task ids restart at 1 every process and would collide), unless
            // resuming: then the resumed session's id is reused so new turns
            // append to the same file.
            let (resume_id, resume_entries) = match resume {
                Some((id, entries)) => (Some(id), Some(entries)),
                None => (None, None),
            };
            let persist = self.persist_root.clone().map(|root| PersistConfig {
                root,
                session_id: resume_id.unwrap_or_else(|| crate::session::new_id_prefixed("sub-")),
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
                        Self::run_on_thread(
                            model,
                            workspace,
                            background,
                            task,
                            Some((sink, source)),
                            persist,
                            resume_entries,
                        )
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

        let model = self.subagent_model.clone();
        let workspace = self.workspace.clone();
        let background = self.background.clone();
        let (resume_id, resume_entries) = match resume {
            Some((id, entries)) => (Some(id), Some(entries)),
            None => (None, None),
        };
        let persist = self.persist_root.clone().map(|root| PersistConfig {
            root,
            session_id: resume_id.unwrap_or_else(|| crate::session::new_id_prefixed("sub-")),
        });
        // Even a synchronous subagent is registered as a running task with a
        // live session, so it shows up in the task panel (F2) and can be
        // attached while the main agent is blocked waiting for it.
        let label = preview(&task, 100);
        let (handle, sink, source) = session_channel();
        let session: Arc<dyn SessionHandle> = Arc::new(handle.clone());
        // A second handle for the cancel guard: dropping the blocked-on
        // `execute` future (the parent turn was cancelled) cancels the
        // subagent, so it cannot outlive its turn as an orphan.
        let cancel_handle: Arc<dyn SessionHandle> = Arc::new(handle.clone());
        let sessions = self.sessions.clone();
        let sessions_in_work = self.sessions.clone();
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None::<u64>));
        let slot_in_hook = slot.clone();
        let slot_in_work = slot.clone();
        // Block until the subagent finishes (this is the synchronous mode).
        // spawn_silent keeps it visible in the task panel without emitting
        // a duplicate completion event (the answer is the tool result).
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<String>();
        let started = self.background.spawn_silent(
            label,
            None,
            move |id| {
                sessions.insert(id, session);
                *slot_in_hook.lock().unwrap() = Some(id);
            },
            move || async move {
                let output = tokio::task::spawn_blocking(move || {
                    Self::run_on_thread(
                        model,
                        workspace,
                        background,
                        task,
                        Some((sink, source)),
                        persist,
                        resume_entries,
                    )
                })
                .await
                .unwrap_or_else(|error| format!("subagent blocking task failed: {error}"));
                if let Some(id) = *slot_in_work.lock().unwrap() {
                    sessions_in_work.remove(id);
                }
                let _ = done_tx.send(output.clone());
                output
            },
        );
        started?;
        // RAII: if this `execute` future is dropped while blocked on the
        // subagent (the parent turn was cancelled), cancel the subagent so
        // it stops instead of running on as an orphan. Normal completion
        // disarms the guard before returning.
        let mut cancel_guard = SubagentCancelGuard::armed(cancel_handle);
        let result = match tokio::time::timeout(SYNC_TIMEOUT, done_rx).await {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(_)) => Err("subagent result channel closed".into()),
            Err(_) => Err("subagent timed out after 30 minutes".into()),
        };
        cancel_guard.disarm();
        result
    }

    fn set_event_sender(&mut self, sender: tokio::sync::mpsc::UnboundedSender<AgentEvent>) {
        self.background.set_event_sender(sender);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::builtins;

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
        // Construct the model with a dummy key directly: no request is ever
        // sent, and tests must not depend on (or mutate) process env.
        let model = ConfiguredModel::Chat(
            crate::model::OpenAiModel::new(
                "http://localhost".into(),
                "test-key".into(),
                "test-model".into(),
                None,
            )
            .unwrap(),
        );
        let (_, background) = builtins(workspace.clone());
        Delegate::new(model, workspace, background)
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
        let (tools, _) = builtins(workspace);
        let names: Vec<String> = tools.iter().map(|tool| tool.spec().name).collect();
        assert!(!names.contains(&"delegate".to_owned()));
        assert!(names.contains(&"bash".to_owned()));
    }

    #[tokio::test]
    async fn shared_background_bash_completion_reaches_the_parent_channel() {
        // A bash tool bound to the parent's BackgroundTasks keeps that
        // sender when wrapped in an Agent (Agent::new must not retarget
        // it): a background command's completion arrives on the parent's
        // channel even after the subagent is dropped. End-to-end subagent
        // behaviour is covered by agent.rs's shared-sender test; here we
        // pin the wiring used by run_on_thread.
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(temp.path()).unwrap();
        let (_, mut parent_background) = builtins(workspace.clone());
        let (parent_sender, mut parent_receiver) =
            tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        parent_background.set_event_sender(parent_sender);

        let started = parent_background
            .start(workspace, "echo shared".into())
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
}
