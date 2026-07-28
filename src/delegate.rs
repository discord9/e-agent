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

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::agent::{Agent, AgentEvent, Tool, ToolSpec, preview};
use crate::model::OpenAiModel;
use crate::tools::builtins;
use crate::workspace::Workspace;

/// Maximum rounds a subagent may take before giving up.
const SUBAGENT_MAX_ROUNDS: usize = 16;
/// Ceiling for a synchronous delegate call.
const SYNC_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub struct Delegate {
    model: OpenAiModel,
    workspace: Workspace,
    sender: Option<mpsc::UnboundedSender<AgentEvent>>,
}

impl Delegate {
    pub fn new(model: OpenAiModel, workspace: Workspace) -> Self {
        Self {
            model,
            workspace,
            sender: None,
        }
    }

    /// Run `task` on a dedicated thread with a fresh agent and return the
    /// final answer. Used by both sync and background execution so the two
    /// modes share one code path.
    fn run_on_thread(model: OpenAiModel, workspace: Workspace, task: String) -> String {
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
            let sender = self
                .sender
                .clone()
                .ok_or("background delegate delivery is unavailable")?;
            let model = self.model.clone();
            let workspace = self.workspace.clone();
            let label = preview(&task, 100);
            // Reuse the next background id for a consistent user-facing label.
            static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1000);
            let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            std::thread::spawn(move || {
                let output = Self::run_on_thread(model, workspace, task);
                let _ = sender.send(AgentEvent::BackgroundCompleted { id, output });
            });
            return Ok(format!("started background task {id}: {label}"));
        }

        let model = self.model.clone();
        let workspace = self.workspace.clone();
        let handle =
            tokio::task::spawn_blocking(move || Self::run_on_thread(model, workspace, task));
        match tokio::time::timeout(SYNC_TIMEOUT, handle).await {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(error)) => Err(format!("subagent thread failed: {error}")),
            Err(_) => Err("subagent timed out after 30 minutes".into()),
        }
    }

    fn set_event_sender(&mut self, sender: mpsc::UnboundedSender<AgentEvent>) {
        self.sender = Some(sender);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delegate(workspace: &std::path::Path) -> Delegate {
        let workspace = Workspace::new(workspace).unwrap();
        let model = OpenAiModel::from_env(None, None).unwrap();
        Delegate::new(model, workspace)
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
    async fn background_requires_event_sender() {
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
