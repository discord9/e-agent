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
//! Future evolution: the thread boundary is deliberately the same shape as a
//! process boundary — swapping `std::thread::spawn` for a spawned
//! `e-agent --subagent` subprocess with a stdio JSONL protocol is the
//! planned path to stronger isolation. MCP tools can be added later by
//! letting the subagent run `mcp::connect_all` inside its own runtime.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::agent::{Agent, AgentEvent, Tool, ToolSpec, preview};
use crate::model::OpenAiModel;
use crate::tools::{BackgroundTasks, builtins};
use crate::workspace::Workspace;

/// Maximum rounds a subagent may take before giving up.
const SUBAGENT_MAX_ROUNDS: usize = 16;
/// Ceiling for a synchronous delegate call.
const SYNC_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Broadcast capacity per subagent session. The TUI view follows in real
/// time; a lagged receiver simply misses events (it holds the history too).
const SESSION_EVENT_CAPACITY: usize = 256;

/// Live view into one running subagent session: a replayable event log plus
/// a broadcast stream of new events. The TUI attach mode snapshots the log
/// on entry and then follows the stream, rebuilding its scrollback the same
/// way the main view does.
#[derive(Clone)]
pub struct SubagentSession {
    log: Arc<Mutex<Vec<AgentEvent>>>,
    events: broadcast::Sender<AgentEvent>,
}

impl SubagentSession {
    fn new() -> Self {
        let (events, _) = broadcast::channel(SESSION_EVENT_CAPACITY);
        Self {
            log: Arc::new(Mutex::new(Vec::new())),
            events,
        }
    }

    /// Snapshot of every event the subagent has emitted so far.
    pub fn snapshot(&self) -> Vec<AgentEvent> {
        self.log.lock().unwrap().clone()
    }

    /// Stream of events from now on (broadcast; lagging misses events).
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.events.subscribe()
    }

    fn emit(&self, event: AgentEvent) {
        self.log.lock().unwrap().push(event.clone());
        // No receivers is fine — the log already holds the record.
        let _ = self.events.send(event);
    }
}

/// Registry of live subagent sessions, keyed by background-task id (the
/// same unified id sequence shared with background bash). Entries are
/// removed when the subagent finishes.
#[derive(Clone, Default)]
pub struct SubagentSessions {
    sessions: Arc<Mutex<std::collections::HashMap<u64, SubagentSession>>>,
}

impl SubagentSessions {
    pub fn get(&self, id: u64) -> Option<SubagentSession> {
        self.sessions.lock().unwrap().get(&id).cloned()
    }

    fn insert(&self, id: u64, session: SubagentSession) {
        self.sessions.lock().unwrap().insert(id, session);
    }

    fn remove(&self, id: u64) {
        self.sessions.lock().unwrap().remove(&id);
    }
}

pub struct Delegate {
    model: OpenAiModel,
    workspace: Workspace,
    /// Shared background slots (with bash): a background delegate occupies
    /// one slot and its completion is delivered as a background completion.
    background: BackgroundTasks,
    /// Live sessions of background subagents, for the TUI attach view.
    sessions: SubagentSessions,
}

impl Delegate {
    pub fn new(model: OpenAiModel, workspace: Workspace, background: BackgroundTasks) -> Self {
        Self {
            model,
            workspace,
            background,
            sessions: SubagentSessions::default(),
        }
    }

    /// Live subagent sessions (background mode only), for attach views.
    pub fn sessions(&self) -> SubagentSessions {
        self.sessions.clone()
    }

    /// Run `task` on a dedicated thread with a fresh agent and return the
    /// final answer. Used by both sync and background execution so the two
    /// modes share one code path. When `session` is given, the subagent's
    /// events are mirrored into it for live viewing (the attach view
    /// rebuilds its scrollback from the event stream).
    fn run_on_thread(
        model: OpenAiModel,
        workspace: Workspace,
        task: String,
        session: Option<SubagentSession>,
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
                if let Some(session) = session {
                    agent.set_event_handler(Box::new(move |event| session.emit(event)));
                }
                match agent.run(task).await {
                    Ok(answer) => answer,
                    Err(error) => format!("subagent failed: {error:#}"),
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
            let session = SubagentSession::new();
            let registered = session.clone();
            let sessions = self.sessions.clone();
            let sessions_in_work = self.sessions.clone();
            let slot = std::sync::Arc::new(std::sync::Mutex::new(None::<u64>));
            let slot_in_hook = slot.clone();
            let slot_in_work = slot.clone();
            // run_on_thread blocks on thread::join, so push it onto the
            // blocking thread pool to keep the executor responsive.
            return self.background.spawn_with_id(
                label,
                None,
                move |id| {
                    sessions.insert(id, registered);
                    *slot_in_hook.lock().unwrap() = Some(id);
                },
                move || async move {
                    let output = tokio::task::spawn_blocking(move || {
                        Self::run_on_thread(model, workspace, task, Some(session))
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
        let handle =
            tokio::task::spawn_blocking(move || Self::run_on_thread(model, workspace, task, None));
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
    fn session_log_replays_and_streams() {
        let session = SubagentSession::new();
        session.emit(AgentEvent::AssistantText("one".into()));
        let mut receiver = session.subscribe();
        session.emit(AgentEvent::AssistantText("two".into()));
        // A late joiner sees the full log plus can follow the stream.
        assert_eq!(
            session.snapshot(),
            vec![
                AgentEvent::AssistantText("one".into()),
                AgentEvent::AssistantText("two".into()),
            ]
        );
        assert_eq!(
            receiver.try_recv().unwrap(),
            AgentEvent::AssistantText("two".into())
        );
    }

    #[test]
    fn registry_tracks_live_sessions() {
        let sessions = SubagentSessions::default();
        assert!(sessions.get(1).is_none());
        sessions.insert(1, SubagentSession::new());
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
