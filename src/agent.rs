use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use tokio::sync::mpsc;

/// Insert a synthetic error result for every tool call left unanswered by an
/// interrupted turn (cancel, provider error, crash), so the derived context
/// always satisfies the provider's tool_call/tool-result pairing rule.
fn repair_tool_pairs(messages: Vec<Message>) -> Vec<Message> {
    fn flush(pending: &mut Vec<ToolCall>, out: &mut Vec<Message>) {
        for call in pending.drain(..) {
            out.push(Message::Tool {
                call_id: call.id,
                name: call.name,
                content: "[turn interrupted before a tool result was produced]".into(),
                is_error: true,
            });
        }
    }

    let mut out = Vec::with_capacity(messages.len());
    let mut pending: Vec<ToolCall> = Vec::new();
    for message in messages {
        match &message {
            Message::Tool { call_id, .. } => {
                pending.retain(|call| &call.id != call_id);
                out.push(message);
            }
            Message::Assistant(assistant) => {
                flush(&mut pending, &mut out);
                pending = assistant.tool_calls.clone();
                out.push(message);
            }
            Message::System { .. } | Message::User { .. } => {
                flush(&mut pending, &mut out);
                out.push(message);
            }
        }
    }
    flush(&mut pending, &mut out);
    out
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Message {
    /// System-prompt-style context (e.g. AGENTS.md or MCP instructions). Sent
    /// to the provider with role "system". Persisted if it ever lands in
    /// history, but the current context prefix is kept out of history.
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant(AssistantMessage),
    Tool {
        call_id: String,
        name: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    /// Model reasoning, persisted for display/audit only. Never sent back
    /// to the provider (see WireMessage).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AgentEvent {
    UserPrompt(String),
    AssistantText(String),
    AssistantDelta(String),
    ReasoningDelta(String),
    ToolCall {
        name: String,
        arguments: String,
    },
    ToolResult {
        is_error: bool,
        content: String,
    },
    /// A system-injected notice (background completion, task-kill report)
    /// rendered in the TUI as a dim line.
    Notice(String),
    /// Emitted on the turn boundary when a background task's completion is
    /// folded into the model context as a `[background task N completed]`
    /// message. Not part of the session event log.
    BackgroundCompleted {
        id: u64,
        output: String,
    },
    Usage {
        /// Input tokens of the most recent regular turn, approximating the
        /// context window currently in use. Compaction calls do not refresh
        /// this (their input is the pre-compaction context).
        context_input: u64,
        /// Cumulative tokens for this process.
        session: Usage,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelDeltaKind {
    Content,
    Reasoning,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// One entry in the append-only session history. The model context is
/// derived from the history: the latest compaction summary plus everything
/// after it. Older entries stay persisted for display/audit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntry {
    Message {
        message: Message,
    },
    Compaction {
        summary: String,
        /// The turn kept verbatim for the model context; not rendered again
        /// in the TUI (it duplicates messages already before this entry).
        retained: Vec<Message>,
    },
    /// A system-injected notice (background completion, task-kill report)
    /// rendered in the TUI as a dim line and surfaced to the model as a
    /// user message.
    Notice {
        text: String,
    },
}

impl From<Message> for SessionEntry {
    fn from(message: Message) -> Self {
        Self::Message { message }
    }
}

/// Token accounting for one provider call, if the provider reports it.

#[async_trait]
pub trait Model: Send {
    async fn complete(
        &mut self,
        messages: &[Message],
        tools: &[ToolSpec],
        on_delta: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)>;

    /// Display name for the UI (e.g. input-box label). Defaults to "?".
    fn name(&self) -> &str {
        "?"
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, arguments: Value) -> Result<String, String>;
    fn set_event_sender(&mut self, _sender: mpsc::UnboundedSender<AgentEvent>) {}
    /// True when the tool already delivers background completions through a
    /// channel of its own (e.g. bound to a shared registry); Agent::new
    /// leaves such tools alone.
    fn has_event_sender(&self) -> bool {
        false
    }
}

pub struct Agent {
    model: Box<dyn Model>,
    tools: Vec<Box<dyn Tool>>,
    history: Vec<SessionEntry>,
    event_handler: Option<Box<dyn FnMut(AgentEvent) + Send>>,
    max_tool_rounds: Option<usize>,
    background_receiver: mpsc::UnboundedReceiver<AgentEvent>,
    pending_background: VecDeque<(u64, String)>,
    subscriber: Option<mpsc::UnboundedSender<AgentEvent>>,
    /// Long-lived session sinks (e.g. a TUI view). Unlike
    /// `event_handler` and `subscriber` (per-turn), these survive across
    /// turns and receive every emitted event. Sinks are dropped once their
    /// session handle is gone.
    observers: Vec<crate::handle::SessionSink>,
    running_background: HashSet<u64>,
    /// Workspace root + session name used to record in-flight background
    /// tasks, so resuming the same session can report what died. None in tests.
    background_record: Option<(std::path::PathBuf, String)>,
    session_input_tokens: u64,
    session_output_tokens: u64,
    last_context_input: u64,
    /// Maximum context window in tokens. When set, triggers auto-compaction
    /// whenever `last_context_input` exceeds 80% of this value.
    context_window: Option<u64>,
    /// Workspace and server instructions prepended to every model call.
    /// Not persisted in sessions.
    context_prefix: Option<String>,
}

impl Agent {
    pub fn new(model: Box<dyn Model>, mut tools: Vec<Box<dyn Tool>>) -> Self {
        let (background_sender, background_receiver) = mpsc::unbounded_channel();
        for tool in &mut tools {
            // Tools already wired to a shared completion channel (e.g. a
            // subagent's bash bound to the parent's BackgroundTasks) keep
            // their sender: their completions belong to the parent agent.
            if !tool.has_event_sender() {
                tool.set_event_sender(background_sender.clone());
            }
        }
        Self {
            model,
            tools,
            history: Vec::new(),
            event_handler: None,
            max_tool_rounds: None,
            background_receiver,
            pending_background: VecDeque::new(),
            subscriber: None,
            observers: Vec::new(),
            running_background: HashSet::new(),
            background_record: None,
            session_input_tokens: 0,
            session_output_tokens: 0,
            last_context_input: 0,
            context_window: None,
            context_prefix: None,
        }
    }

    /// Extra system context prepended to every model call. Not persisted in
    /// sessions.
    pub fn set_context_prefix(&mut self, prefix: String) {
        self.context_prefix = Some(prefix);
    }

    /// Record in-flight background tasks under this workspace root + session,
    /// so resuming the same session later can report what died with this process.
    pub fn record_background_tasks_in(&mut self, root: std::path::PathBuf, session: &str) {
        self.background_record = Some((root, session.to_owned()));
    }

    /// Cap the number of tool-call rounds per turn. None (the default) means
    /// unlimited: a turn runs until the model stops calling tools.
    pub fn max_tool_rounds(mut self, rounds: usize) -> Self {
        self.max_tool_rounds = Some(rounds);
        self
    }

    /// Set the context window (token count). When set, the agent
    /// auto-compacts when usage exceeds 80% of this value.
    pub fn set_context_window(&mut self, window: u64) {
        self.context_window = Some(window);
    }

    pub fn set_event_handler(&mut self, handler: Box<dyn FnMut(AgentEvent) + Send>) {
        self.event_handler = Some(handler);
    }

    /// Full append-only history (what is persisted and shown in the TUI).
    pub fn history(&self) -> &[SessionEntry] {
        &self.history
    }

    /// Replace the whole history (session resume). To ADD one entry to an
    /// already-loaded history, use [`Self::push_entry`] — calling this again
    /// would wipe the restored entries.
    pub fn restore_history(&mut self, history: Vec<SessionEntry>) {
        self.history = history;
    }

    /// Append a single entry to the history (e.g. a startup notice injected
    /// after resume).
    pub fn push_entry(&mut self, entry: SessionEntry) {
        self.history.push(entry);
    }

    /// Messages sent to the provider: the latest compaction summary plus
    /// everything after it.
    pub fn context(&self) -> Vec<Message> {
        let mut messages = Vec::new();
        if let Some(prefix) = &self.context_prefix {
            messages.push(Message::System {
                content: prefix.clone(),
            });
        }
        let mut start = 0;
        if let Some(index) = self
            .history
            .iter()
            .rposition(|entry| matches!(entry, SessionEntry::Compaction { .. }))
        {
            let SessionEntry::Compaction { summary, retained } = &self.history[index] else {
                unreachable!()
            };
            messages.push(Message::User {
                content: format!("[compacted summary of earlier conversation]\n{summary}"),
            });
            messages.extend(retained.iter().cloned());
            start = index + 1;
        }
        messages.extend(
            self.history[start..]
                .iter()
                .filter_map(|entry| match entry {
                    SessionEntry::Message { message } => Some(message.clone()),
                    SessionEntry::Compaction { .. } => None,
                    // Notices are system-injected events; surface them to the
                    // model as user messages so the model reacts to background
                    // completions and task-death notices.
                    SessionEntry::Notice { text } => Some(Message::User {
                        content: text.clone(),
                    }),
                }),
        );
        repair_tool_pairs(messages)
    }

    pub fn subscribe(&mut self, sender: mpsc::UnboundedSender<AgentEvent>) {
        self.subscriber = Some(sender);
    }

    /// Register a long-lived session sink receiving every event (text,
    /// tool calls, background completions) across all turns. The matching
    /// `LiveSession` handle lets a frontend snapshot and follow this agent
    /// without owning it.
    pub fn observe(&mut self, sink: crate::handle::SessionSink) {
        self.observers.push(sink);
    }

    fn fanout(&self, event: &AgentEvent) {
        for sink in &self.observers {
            sink.emit(event.clone());
        }
    }

    pub fn background_task_ids(&self) -> &HashSet<u64> {
        &self.running_background
    }

    /// Names of the registered tools (for tests and diagnostics).
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.iter().map(|tool| tool.spec().name).collect()
    }

    /// Wait for the next background task completion. Used by the TUI to
    /// wake an idle agent. The event is also queued for injection into the
    /// next model call.
    pub async fn next_background_completion(&mut self) -> Option<(u64, String)> {
        loop {
            match self.background_receiver.recv().await {
                Some(AgentEvent::BackgroundCompleted { id, output }) => {
                    self.running_background.remove(&id);
                    if let Some((root, session)) = &self.background_record {
                        crate::session::Session::clear_background_task(root, session, id);
                    }
                    self.pending_background.push_back((id, output.clone()));
                    // No fanout here either: idle and mid-turn completions
                    // both land in the session log as a user message at the
                    // next turn boundary. The TUI prints this return value
                    // itself; fanning out would duplicate the line.
                    return Some((id, output));
                }
                Some(_) => {}
                None => return None,
            }
        }
    }

    /// Run turns until the model has reacted to every background
    /// completion: completions arriving mid-turn are folded into the
    /// history at the turn's end, and this loop immediately starts a
    /// follow-up turn so the model reacts instead of waiting for the next
    /// user prompt. Returns the last turn's answer.
    pub async fn run(&mut self, prompt: String) -> anyhow::Result<String> {
        let mut prompt = prompt;
        loop {
            let (answer, injected_at_end) = self.run_turn(std::mem::take(&mut prompt)).await?;
            if !injected_at_end {
                self.subscriber = None;
                return Ok(answer);
            }
        }
    }

    /// One turn. The returned bool is true when completions that arrived
    /// mid-turn were folded into the history at the turn's end without the
    /// model reacting to them yet — run() uses it to start a follow-up turn.
    async fn run_turn(&mut self, prompt: String) -> anyhow::Result<(String, bool)> {
        self.drain_background();
        self.inject_pending_background();
        if !prompt.is_empty() {
            self.history.push(Message::User { content: prompt }.into());
        }
        let specs: Vec<_> = self.tools.iter().map(|tool| tool.spec()).collect();

        let result = self.run_loop(&specs).await;
        self.drain_background();
        // Completions that arrived during this turn were drained into
        // pending but the loop ended before injecting them; fold them into
        // the history now so the finished line renders immediately instead
        // of waiting for the next prompt. run() then loops so the model
        // reacts to them right away.
        let injected_at_end = !self.pending_background.is_empty();
        self.inject_pending_background();
        result.map(|answer| (answer, injected_at_end))
    }

    /// Summarize everything before the current turn and append it as a
    /// compaction entry. The current turn is kept verbatim inside the entry
    /// so the derived context still sees it, while the full history stays
    /// append-only.
    pub async fn compact(&mut self) -> anyhow::Result<String> {
        let context = self.context();
        let Some(split) = context
            .iter()
            .rposition(|message| matches!(message, Message::User { .. }))
        else {
            anyhow::bail!("nothing to compact");
        };
        if split == 0 {
            anyhow::bail!("nothing to compact");
        }
        let mut request = context[..split].to_vec();
        request.push(Message::User {
            content: "Summarize the earlier conversation. Preserve the user's goals, decisions made, files changed, and unfinished work. Be concise and use Chinese or English to match the conversation language.".into(),
        });
        let response = {
            let model = &mut self.model;
            let event_handler = &mut self.event_handler;
            let observers = &self.observers;
            let mut on_delta = |kind: ModelDeltaKind, delta: &str| {
                let event = match kind {
                    ModelDeltaKind::Content => AgentEvent::AssistantDelta(delta.into()),
                    ModelDeltaKind::Reasoning => AgentEvent::ReasoningDelta(delta.into()),
                };
                if let Some(handler) = event_handler {
                    handler(event.clone());
                }
                // Compaction streams to the session log too: frontends
                // watching via a SessionHandle see the summary appear live
                // instead of popping in whole at the end.
                for sink in observers {
                    sink.emit(event.clone());
                }
            };
            model.complete(&request, &[], Some(&mut on_delta)).await?
        };
        let (response, usage) = response;
        self.record_usage(usage, false);
        let summary = response.content.unwrap_or_default();
        self.history.push(SessionEntry::Compaction {
            summary: summary.clone(),
            retained: context[split..].to_vec(),
        });
        Ok(summary)
    }

    fn push_message(&mut self, message: Message) {
        self.history.push(message.into());
    }

    fn record_usage(&mut self, usage: Option<Usage>, refresh_context: bool) {
        if let Some(usage) = usage {
            self.session_input_tokens += usage.input_tokens;
            self.session_output_tokens += usage.output_tokens;
            if refresh_context {
                self.last_context_input = usage.input_tokens;
            }
            self.emit(AgentEvent::Usage {
                context_input: self.last_context_input,
                session: Usage {
                    input_tokens: self.session_input_tokens,
                    output_tokens: self.session_output_tokens,
                },
            });
        }
    }

    async fn run_loop(&mut self, specs: &[ToolSpec]) -> anyhow::Result<String> {
        let mut rounds = 0usize;
        loop {
            if let Some(limit) = self.max_tool_rounds
                && rounds >= limit
            {
                anyhow::bail!("tool call limit ({limit}) reached");
            }
            rounds += 1;
            self.drain_background();
            self.inject_pending_background();
            let mut produced_delta = false;
            let context = self.context();
            let assistant = {
                let model = &mut self.model;
                let event_handler = &mut self.event_handler;
                let observers = &self.observers;
                let mut on_delta = |kind: ModelDeltaKind, delta: &str| {
                    if kind == ModelDeltaKind::Content {
                        produced_delta = true;
                    }
                    let event = match kind {
                        ModelDeltaKind::Content => AgentEvent::AssistantDelta(delta.into()),
                        ModelDeltaKind::Reasoning => AgentEvent::ReasoningDelta(delta.into()),
                    };
                    if let Some(handler) = event_handler {
                        handler(event.clone());
                    }
                    // Subagents stream to their session log too, so an
                    // attached view shows live thinking/output instead of a
                    // frozen screen during a long reasoning call.
                    for sink in observers {
                        sink.emit(event.clone());
                    }
                };
                model.complete(&context, specs, Some(&mut on_delta)).await?
            };
            let (assistant, usage) = assistant;
            self.record_usage(usage, true);
            // Auto-compact when usage exceeds 80% of the configured context window.
            if let Some(window) = self.context_window
                && window > 0
                && self.last_context_input >= window.saturating_mul(80) / 100
            {
                self.emit(AgentEvent::Notice("──── auto-compacting… ────".into()));
                if let Err(error) = self.compact().await {
                    self.emit(AgentEvent::Notice(format!(
                        "auto-compaction error: {error:#}"
                    )));
                }
                self.emit(AgentEvent::Notice("──── auto-compaction ────".into()));
            }
            if assistant.tool_calls.is_empty() {
                let answer = assistant.content.clone().unwrap_or_default();
                self.push_message(Message::Assistant(assistant));
                return Ok(answer);
            }

            if !produced_delta
                && let Some(content) = assistant
                    .content
                    .as_deref()
                    .filter(|content| !content.is_empty())
            {
                self.emit(AgentEvent::AssistantText(content.into()));
            }
            self.push_message(Message::Assistant(assistant.clone()));
            for call in &assistant.tool_calls {
                self.emit(AgentEvent::ToolCall {
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                });
                let result = Self::execute_on(&self.tools, call).await;
                if result.is_ok()
                    && call.name == "bash"
                    && is_background_call(call)
                    && let Some(id) = started_task_id(result.as_deref().unwrap_or_default())
                {
                    self.running_background.insert(id);
                    if let Some((root, session)) = &self.background_record {
                        let command = serde_json::from_str::<Value>(&call.arguments)
                            .ok()
                            .and_then(|args| args["command"].as_str().map(str::to_owned))
                            .unwrap_or_else(|| call.arguments.clone());
                        let label = crate::agent::preview(&command, 100);
                        let _ = crate::session::Session::record_background_start(
                            root, session, id, &label, None,
                        );
                    }
                }
                self.emit(AgentEvent::ToolResult {
                    is_error: result.is_err(),
                    content: match &result {
                        Ok(content) | Err(content) => content.clone(),
                    },
                });
                self.push_message(Message::Tool {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    content: match &result {
                        Ok(content) | Err(content) => content.clone(),
                    },
                    is_error: result.is_err(),
                });
            }
        }
    }

    /// Execute a tool call against a tool list. Associated function (not a
    /// method) so the returned future does not borrow `&self`, keeping
    /// `Agent::run` futures `Send` for use in `tokio::spawn`.
    async fn execute_on(tools: &[Box<dyn Tool>], call: &ToolCall) -> Result<String, String> {
        let arguments = serde_json::from_str(&call.arguments)
            .map_err(|error| format!("invalid JSON arguments: {error}"))?;
        let Some(tool) = tools.iter().find(|tool| tool.spec().name == call.name) else {
            return Err(format!("unknown tool: {}", call.name));
        };
        tool.execute(arguments).await
    }

    fn emit(&mut self, event: AgentEvent) {
        if let Some(handler) = &mut self.event_handler {
            handler(event.clone());
        }
        self.fanout(&event);
    }

    fn inject_pending_background(&mut self) {
        while let Some((id, output)) = self.pending_background.pop_front() {
            let text = format!("[background task {id} completed]\n{output}");
            // The completion joins the history as a structured Notice entry;
            // emit it to the session observers only (their event log is the
            // TUI's scrollback and the attach snapshot). The per-turn
            // subscriber is for transient signals (deltas, the
            // BackgroundCompleted fired at drain time) — sending this too
            // would render the line twice in the TUI, which listens to both.
            self.fanout(&AgentEvent::Notice(text.clone()));
            self.history.push(SessionEntry::Notice { text });
        }
    }

    fn drain_background(&mut self) {
        while let Ok(AgentEvent::BackgroundCompleted { id, output }) =
            self.background_receiver.try_recv()
        {
            self.running_background.remove(&id);
            // The task finished while we were alive; forget its record.
            if let Some((root, session)) = &self.background_record {
                crate::session::Session::clear_background_task(root, session, id);
            }
            self.pending_background.push_back((id, output.clone()));
            // Notify only the per-turn subscriber (the TUI's live display).
            // The long-lived observers' log gets this completion at the
            // turn boundary, when `pending_background` is folded into a
            // `[background task N completed]` user message — emitting here
            // too would surface it twice.
            if let Some(subscriber) = &self.subscriber {
                let _ = subscriber.send(AgentEvent::BackgroundCompleted { id, output });
            }
        }
    }
}

fn is_background_call(call: &ToolCall) -> bool {
    serde_json::from_str::<Value>(&call.arguments)
        .ok()
        .and_then(|value| value.get("background").and_then(Value::as_bool))
        .unwrap_or(false)
}

fn started_task_id(output: &str) -> Option<u64> {
    output
        .strip_prefix("started background task ")?
        .split_once(':')?
        .0
        .parse()
        .ok()
}

pub fn preview(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;
    use crate::handle::SessionHandle;

    struct ScriptedModel {
        replies: Vec<AssistantMessage>,
        requests: Arc<Mutex<Vec<Vec<Message>>>>,
        /// Optional per-call latencies (popped in order; exhausted = no
        /// delay), to let a background completion land while a specific
        /// model call is in flight.
        delays: std::collections::VecDeque<Option<std::time::Duration>>,
    }

    #[async_trait]
    impl Model for ScriptedModel {
        async fn complete(
            &mut self,
            messages: &[Message],
            _: &[ToolSpec],
            _: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
        ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
            self.requests.lock().unwrap().push(messages.to_vec());
            if let Some(Some(delay)) = self.delays.pop_front() {
                tokio::time::sleep(delay).await;
            }
            Ok((self.replies.remove(0), None))
        }
    }

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo".into(),
                description: "echoes input".into(),
                parameters: json!({"type": "object"}),
            }
        }

        async fn execute(&self, arguments: Value) -> Result<String, String> {
            Ok(arguments["value"].to_string())
        }
    }

    struct FailingTool;

    #[async_trait]
    impl Tool for FailingTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "fail".into(),
                description: "always fails".into(),
                parameters: json!({"type": "object"}),
            }
        }

        async fn execute(&self, _: Value) -> Result<String, String> {
            Err("execution failed".into())
        }
    }

    struct ScriptedBackgroundTool {
        sender: Option<mpsc::UnboundedSender<AgentEvent>>,
    }

    #[async_trait]
    impl Tool for ScriptedBackgroundTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "bash".into(),
                description: "background test".into(),
                parameters: json!({"type": "object"}),
            }
        }

        async fn execute(&self, arguments: Value) -> Result<String, String> {
            assert_eq!(arguments["background"], true);
            let sender = self.sender.clone().unwrap();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                let _ = sender.send(AgentEvent::BackgroundCompleted {
                    id: 1,
                    output: "exit code: 0\nstdout:\ndone\nstderr:\n".into(),
                });
            });
            Ok("started background task 1: echo done".into())
        }

        fn set_event_sender(&mut self, sender: mpsc::UnboundedSender<AgentEvent>) {
            self.sender = Some(sender);
        }
    }

    struct SlowEchoTool;

    #[async_trait]
    impl Tool for SlowEchoTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "slow_echo".into(),
                description: "sleeps, then echoes".into(),
                parameters: json!({"type": "object"}),
            }
        }

        async fn execute(&self, arguments: Value) -> Result<String, String> {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok(arguments["value"].to_string())
        }
    }

    struct DeltaModel {
        calls: usize,
    }

    #[async_trait]
    impl Model for DeltaModel {
        async fn complete(
            &mut self,
            _: &[Message],
            _: &[ToolSpec],
            mut on_delta: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
        ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
            self.calls += 1;
            if self.calls == 1 {
                if let Some(callback) = &mut on_delta {
                    callback(ModelDeltaKind::Reasoning, "thinking");
                    callback(ModelDeltaKind::Content, "streamed");
                }
                return Ok((
                    AssistantMessage {
                        content: Some("streamed".into()),
                        tool_calls: vec![call("call-1", "echo", r#"{"value":"ok"}"#)],
                        reasoning: None,
                    },
                    None,
                ));
            }
            Ok((
                AssistantMessage {
                    content: Some("final".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
                None,
            ))
        }
    }

    fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    #[tokio::test]
    async fn feeds_assistant_calls_and_results_back_to_the_model() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let model = ScriptedModel {
            replies: vec![
                AssistantMessage {
                    content: None,
                    tool_calls: vec![call("call-1", "echo", r#"{"value":"ok"}"#)],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("final answer".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
            ],
            requests: requests.clone(),
            delays: Default::default(),
        };
        let mut agent = Agent::new(Box::new(model), vec![Box::new(EchoTool)]);

        assert_eq!(agent.run("hello".into()).await.unwrap(), "final answer");
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(matches!(
            &requests[1][1],
            Message::Assistant(message) if message.tool_calls == vec![call("call-1", "echo", r#"{"value":"ok"}"#)]
        ));
        assert!(matches!(
            &requests[1][2],
            Message::Tool { call_id, name, content, is_error: false }
                if call_id == "call-1" && name == "echo" && content == "\"ok\""
        ));
    }

    #[tokio::test]
    async fn keeps_transcript_across_runs() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let model = ScriptedModel {
            replies: vec![
                AssistantMessage {
                    content: Some("first".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("second".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
            ],
            requests: requests.clone(),
            delays: Default::default(),
        };
        let mut agent = Agent::new(Box::new(model), vec![]);

        assert_eq!(agent.run("one".into()).await.unwrap(), "first");
        assert_eq!(agent.run("two".into()).await.unwrap(), "second");
        let requests = requests.lock().unwrap();
        assert_eq!(requests[0].len(), 1);
        assert_eq!(requests[1].len(), 3);
        assert!(matches!(
            &requests[1][0],
            Message::User { content } if content == "one"
        ));
        assert!(matches!(
            &requests[1][1],
            Message::Assistant(message) if message.content.as_deref() == Some("first")
        ));
        assert!(matches!(
            &requests[1][2],
            Message::User { content } if content == "two"
        ));
    }

    #[tokio::test]
    async fn returns_invalid_arguments_and_execution_failures_to_the_model() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let model = ScriptedModel {
            replies: vec![
                AssistantMessage {
                    content: None,
                    tool_calls: vec![call("bad-json", "fail", "not json")],
                    reasoning: None,
                },
                AssistantMessage {
                    content: None,
                    tool_calls: vec![call("failed", "fail", "{}")],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("recovered".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
            ],
            requests: requests.clone(),
            delays: Default::default(),
        };
        let mut agent = Agent::new(Box::new(model), vec![Box::new(FailingTool)]);

        assert_eq!(agent.run("hello".into()).await.unwrap(), "recovered");
        let requests = requests.lock().unwrap();
        assert!(matches!(
            &requests[1][2],
            Message::Tool { is_error: true, content, .. } if content.contains("invalid JSON")
        ));
        assert!(matches!(
            &requests[2][4],
            Message::Tool { is_error: true, content, .. } if content == "execution failed"
        ));
    }

    #[tokio::test]
    async fn emits_assistant_tool_and_result_events_in_order() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let model = ScriptedModel {
            replies: vec![
                AssistantMessage {
                    content: Some("working".into()),
                    tool_calls: vec![call("call-1", "echo", r#"{"value":"ok"}"#)],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("final".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
            ],
            requests,
            delays: Default::default(),
        };
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut agent = Agent::new(Box::new(model), vec![Box::new(EchoTool)]);
        let captured = events.clone();
        agent.set_event_handler(Box::new(move |event| captured.lock().unwrap().push(event)));

        assert_eq!(agent.run("hello".into()).await.unwrap(), "final");
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                AgentEvent::AssistantText("working".into()),
                AgentEvent::ToolCall {
                    name: "echo".into(),
                    arguments: r#"{"value":"ok"}"#.into(),
                },
                AgentEvent::ToolResult {
                    is_error: false,
                    content: "\"ok\"".into(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn observed_events_survive_without_any_subscriber() {
        // Regression: sinks used to be dropped when no broadcast receiver
        // existed (TUI not attached yet), wiping the session log — an
        // attach then saw an empty snapshot.
        let (handle, sink, _source) = crate::handle::session_channel();
        use crate::handle::SessionHandle as _;
        let model = ScriptedModel {
            replies: vec![
                AssistantMessage {
                    content: Some("working".into()),
                    tool_calls: vec![call("call-1", "echo", r#"{"value":"ok"}"#)],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("done".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
            ],
            requests: Arc::new(Mutex::new(Vec::new())),
            delays: Default::default(),
        };
        let mut agent = Agent::new(Box::new(model), vec![Box::new(EchoTool)]);
        agent.observe(sink);
        // No handle.subscribe() call: nobody is listening live.
        assert_eq!(agent.run("hello".into()).await.unwrap(), "done");
        assert_eq!(
            handle.snapshot(),
            vec![
                AgentEvent::AssistantText("working".into()),
                AgentEvent::ToolCall {
                    name: "echo".into(),
                    arguments: r#"{"value":"ok"}"#.into(),
                },
                AgentEvent::ToolResult {
                    is_error: false,
                    content: "\"ok\"".into(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn emits_deltas_without_duplicate_assistant_text() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut agent = Agent::new(Box::new(DeltaModel { calls: 0 }), vec![Box::new(EchoTool)]);
        let captured = events.clone();
        agent.set_event_handler(Box::new(move |event| captured.lock().unwrap().push(event)));
        assert_eq!(agent.run("hello".into()).await.unwrap(), "final");
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                AgentEvent::ReasoningDelta("thinking".into()),
                AgentEvent::AssistantDelta("streamed".into()),
                AgentEvent::ToolCall {
                    name: "echo".into(),
                    arguments: r#"{"value":"ok"}"#.into(),
                },
                AgentEvent::ToolResult {
                    is_error: false,
                    content: "\"ok\"".into(),
                },
            ]
        );
        assert!(agent.context().iter().all(|message| !matches!(
            message,
            Message::Assistant(AssistantMessage { content: Some(content), .. }) if content.contains("thinking")
        )));
    }

    #[tokio::test]
    async fn background_completion_notifies_subscriber_once_and_observers_at_turn_boundary() {
        // Regression: drain_background used to fanout to observers AND the
        // completion reappeared in the model context as a user message, so
        // the TUI (which listens to both paths) showed it twice. Now the
        // transient BackgroundCompleted goes only to the per-turn
        // subscriber and the observer log gets it as UserPrompt at the
        // turn boundary.
        let model = ScriptedModel {
            replies: vec![
                AssistantMessage {
                    content: None,
                    tool_calls: vec![call(
                        "background-1",
                        "bash",
                        r#"{"command":"echo done","background":true}"#,
                    )],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("started".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("next".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
            ],
            requests: Arc::new(Mutex::new(Vec::new())),
            delays: Default::default(),
        };
        let mut agent = Agent::new(
            Box::new(model),
            vec![Box::new(ScriptedBackgroundTool { sender: None })],
        );
        let (handle, sink, _source) = crate::handle::session_channel();
        agent.observe(sink);
        assert_eq!(agent.run("first".into()).await.unwrap(), "started");
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        // Completion lands while idle: no observer event yet.
        assert!(
            handle
                .snapshot()
                .iter()
                .all(|event| !matches!(event, AgentEvent::BackgroundCompleted { .. }))
        );
        let (sender, mut receiver) = mpsc::unbounded_channel();
        agent.subscribe(sender);
        assert_eq!(agent.run("second".into()).await.unwrap(), "next");
        // Subscriber saw the transient completion exactly once, and the
        // turn-boundary notice is NOT sent to it (that would render twice
        // in the TUI, which listens to both paths).
        let mut transient = 0;
        let mut notice_notifications = 0;
        while let Ok(event) = receiver.try_recv() {
            match event {
                AgentEvent::BackgroundCompleted { id: 1, .. } => transient += 1,
                AgentEvent::Notice(text) if text.starts_with("[background task 1 completed]") => {
                    notice_notifications += 1
                }
                _ => {}
            }
        }
        assert_eq!(transient, 1);
        assert_eq!(notice_notifications, 0);
        // ...and the observer log holds it once, as the Notice, never as
        // the transient variant.
        let snapshot = handle.snapshot();
        assert!(
            snapshot
                .iter()
                .all(|event| !matches!(event, AgentEvent::BackgroundCompleted { .. }))
        );
        assert_eq!(
            snapshot
                .iter()
                .filter(|event| matches!(event, AgentEvent::Notice(text) if text.starts_with("[background task 1 completed]")))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn injects_background_completion_before_the_next_prompt_and_forwards_it() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let model = ScriptedModel {
            replies: vec![
                AssistantMessage {
                    content: None,
                    tool_calls: vec![call(
                        "background-1",
                        "bash",
                        r#"{"command":"echo done","background":true}"#,
                    )],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("started".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("next".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
            ],
            requests: requests.clone(),
            delays: Default::default(),
        };
        let mut agent = Agent::new(
            Box::new(model),
            vec![Box::new(ScriptedBackgroundTool { sender: None })],
        );
        assert_eq!(agent.run("first".into()).await.unwrap(), "started");
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let (sender, mut receiver) = mpsc::unbounded_channel();
        agent.subscribe(sender);
        assert_eq!(agent.run("second".into()).await.unwrap(), "next");
        assert!(matches!(
            receiver.try_recv(),
            Ok(AgentEvent::BackgroundCompleted { id: 1, .. })
        ));
        let requests = requests.lock().unwrap();
        assert!(matches!(
            &requests[1][2],
            Message::Tool { content, is_error: false, .. } if content.starts_with("started background task 1:")
        ));
        // The completion is injected as a Notice, which context() surfaces
        // as a Message::User so the model sees it.
        assert!(matches!(
            &requests[2][4],
            Message::User { content } if content.starts_with("[background task 1 completed]\n")
        ));
        assert!(matches!(
            &requests[2][5],
            Message::User { content } if content == "second"
        ));
    }

    #[tokio::test]
    async fn injects_background_completion_mid_loop_before_the_next_model_call() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let model = ScriptedModel {
            replies: vec![
                AssistantMessage {
                    content: None,
                    tool_calls: vec![call(
                        "background-1",
                        "bash",
                        r#"{"command":"echo done","background":true}"#,
                    )],
                    reasoning: None,
                },
                AssistantMessage {
                    content: None,
                    tool_calls: vec![call("call-2", "slow_echo", r#"{"value":"ok"}"#)],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("done".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
            ],
            requests: requests.clone(),
            delays: Default::default(),
        };
        let mut agent = Agent::new(
            Box::new(model),
            vec![
                Box::new(ScriptedBackgroundTool { sender: None }),
                Box::new(SlowEchoTool),
            ],
        );

        assert_eq!(agent.run("go".into()).await.unwrap(), "done");
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(matches!(
            requests[2].last().unwrap(),
            Message::User { content } if content.starts_with("[background task 1 completed]\n")
        ));
    }

    #[tokio::test]
    async fn completion_arriving_during_the_final_tool_call_gets_a_follow_up_turn() {
        // Regression: a completion that landed while the LAST tool call of a
        // turn was still executing was drained into pending but never
        // injected (run_loop had no next round), so the finished line and
        // the model's reaction both waited for the next user prompt. run()
        // must inject at turn end and immediately run a follow-up turn so
        // the model reacts to the completion.
        let requests = Arc::new(Mutex::new(Vec::new()));
        let model = ScriptedModel {
            replies: vec![
                // Round 1: launch the background task.
                AssistantMessage {
                    content: None,
                    tool_calls: vec![call(
                        "background-1",
                        "bash",
                        r#"{"command":"echo done","background":true}"#,
                    )],
                    reasoning: None,
                },
                // Round 2: the model finishes with no tool call. Its call
                // is delayed past the background completion (10ms), which
                // therefore lands mid-turn; with no further rounds, only
                // turn-end injection keeps it from stranding until the next
                // prompt.
                AssistantMessage {
                    content: Some("done".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
                // Follow-up turn: the model reacts to the injected
                // completion.
                AssistantMessage {
                    content: Some("acknowledged".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
            ],
            requests: requests.clone(),
            // Only round 2 (the final answer) is delayed, so the 10ms
            // completion lands during the LAST model call of the turn
            // instead of being picked up by a later round's inject.
            delays: vec![None, Some(std::time::Duration::from_millis(30))].into(),
        };
        let mut agent = Agent::new(
            Box::new(model),
            vec![Box::new(ScriptedBackgroundTool { sender: None })],
        );
        let (handle, sink, _source) = crate::handle::session_channel();
        use crate::handle::SessionHandle;
        agent.observe(sink);

        // run() keeps going until the model has reacted to the completion;
        // the returned answer is the follow-up turn's.
        assert_eq!(agent.run("go".into()).await.unwrap(), "acknowledged");
        // The completion is in the history as a Notice entry...
        assert!(agent.history().iter().any(|entry| matches!(
            entry,
            crate::agent::SessionEntry::Notice { text } if text.starts_with("[background task 1 completed]\n")
        )));
        // ...the observer log shows it exactly once (the finished line)...
        assert_eq!(
            handle
                .snapshot()
                .iter()
                .filter(|event| matches!(event, AgentEvent::Notice(t) if t.starts_with("[background task 1 completed]")))
                .count(),
            1
        );
        // ...and the follow-up turn's request carried it to the model.
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3, "requests: {requests:#?}");
        assert!(matches!(
            requests[2].last().unwrap(),
            Message::User { content } if content.starts_with("[background task 1 completed]\n")
        ));
    }

    #[tokio::test]
    async fn compacts_everything_before_the_current_turn() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let model = ScriptedModel {
            replies: vec![AssistantMessage {
                content: Some("summary text".into()),
                tool_calls: vec![],
                reasoning: None,
            }],
            requests: requests.clone(),
            delays: Default::default(),
        };
        let tool_call = call("call-1", "echo", r#"{"value":"old"}"#);
        let current_turn = vec![
            Message::User {
                content: "recent request".into(),
            },
            Message::Assistant(AssistantMessage {
                content: None,
                tool_calls: vec![
                    call("call-2", "bash", r#"{"command":"make"}"#),
                    call("call-3", "bash", r#"{"command":"make test"}"#),
                ],
                reasoning: None,
            }),
            Message::Tool {
                call_id: "call-2".into(),
                name: "bash".into(),
                content: "building".into(),
                is_error: false,
            },
            Message::Tool {
                call_id: "call-3".into(),
                name: "bash".into(),
                content: "still building".into(),
                is_error: false,
            },
            Message::Assistant(AssistantMessage {
                content: Some("recent answer".into()),
                tool_calls: vec![],
                reasoning: None,
            }),
        ];
        let mut transcript = vec![
            Message::User {
                content: "original goal".into(),
            },
            Message::Assistant(AssistantMessage {
                content: None,
                tool_calls: vec![tool_call.clone()],
                reasoning: None,
            }),
            Message::Tool {
                call_id: tool_call.id,
                name: "echo".into(),
                content: "old result".into(),
                is_error: false,
            },
            Message::User {
                content: "follow up".into(),
            },
            Message::Assistant(AssistantMessage {
                content: Some("noted".into()),
                tool_calls: vec![],
                reasoning: None,
            }),
        ];
        transcript.extend(current_turn.clone());
        let mut agent = Agent::new(Box::new(model), vec![]);
        agent.restore_history(transcript.into_iter().map(Into::into).collect());

        assert_eq!(agent.compact().await.unwrap(), "summary text");
        // Full history is append-only: 10 original entries + 1 compaction.
        assert_eq!(agent.history().len(), 11);
        assert!(matches!(
            agent.history().last().unwrap(),
            SessionEntry::Compaction { summary, retained }
                if summary == "summary text" && *retained == current_turn
        ));
        // The derived context is the summary plus the retained current turn.
        let context = agent.context();
        assert_eq!(context.len(), current_turn.len() + 1);
        assert!(matches!(
            &context[0],
            Message::User { content } if content == "[compacted summary of earlier conversation]\nsummary text"
        ));
        assert_eq!(&context[1..], current_turn.as_slice());
        let requests = requests.lock().unwrap();
        assert_eq!(requests[0].len(), 6);
        assert!(matches!(
            requests[0].last().unwrap(),
            Message::User { content } if content.contains("Summarize the earlier conversation")
        ));
    }

    #[tokio::test]
    async fn context_repairs_unanswered_tool_calls_from_interrupted_turns() {
        let interrupted = vec![
            Message::User {
                content: "do things".into(),
            },
            Message::Assistant(AssistantMessage {
                content: None,
                tool_calls: vec![
                    call("call-1", "bash", r#"{"command":"make"}"#),
                    call("call-2", "bash", r#"{"command":"make test"}"#),
                ],
                reasoning: None,
            }),
            Message::Tool {
                call_id: "call-1".into(),
                name: "bash".into(),
                content: "built".into(),
                is_error: false,
            },
        ];
        let mut agent = Agent::new(
            Box::new(ScriptedModel {
                replies: vec![],
                requests: Arc::new(Mutex::new(Vec::new())),
                delays: Default::default(),
            }),
            vec![],
        );
        agent.restore_history(interrupted.into_iter().map(Into::into).collect());
        let context = agent.context();
        assert_eq!(context.len(), 4);
        assert!(matches!(
            &context[3],
            Message::Tool { call_id, is_error: true, content, .. }
                if call_id == "call-2" && content.contains("interrupted")
        ));
    }

    #[tokio::test]
    async fn refuses_to_compact_a_too_short_transcript() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let model = ScriptedModel {
            replies: vec![],
            requests,
            delays: Default::default(),
        };
        let mut agent = Agent::new(Box::new(model), vec![]);
        agent.restore_history(vec![
            Message::User {
                content: "short".into(),
            }
            .into(),
        ]);
        assert!(
            agent
                .compact()
                .await
                .unwrap_err()
                .to_string()
                .contains("nothing to compact")
        );
    }

    struct CompactDeltaModel;

    #[async_trait]
    impl Model for CompactDeltaModel {
        async fn complete(
            &mut self,
            _: &[Message],
            _: &[ToolSpec],
            mut on_delta: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
        ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
            if let Some(callback) = &mut on_delta {
                callback(ModelDeltaKind::Content, "sum");
                callback(ModelDeltaKind::Content, "mary");
            }
            Ok((
                AssistantMessage {
                    content: Some("summary".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
                None,
            ))
        }
    }

    #[tokio::test]
    async fn compact_streams_deltas_to_observers() {
        // The TUI renders /compact through the session observer: without
        // fanout here the summary pops in whole at the end instead of
        // streaming live between the compaction banners.
        let (handle, sink, _source) = crate::handle::session_channel();
        use crate::handle::SessionHandle as _;
        let mut agent = Agent::new(Box::new(CompactDeltaModel), vec![]);
        agent.observe(sink);
        agent.restore_history(vec![
            Message::User {
                content: "one".into(),
            }
            .into(),
            Message::Assistant(AssistantMessage {
                content: Some("two".into()),
                tool_calls: vec![],
                reasoning: None,
            })
            .into(),
            Message::User {
                content: "current turn".into(),
            }
            .into(),
        ]);
        assert_eq!(agent.compact().await.unwrap(), "summary");
        assert_eq!(
            handle.snapshot(),
            vec![
                AgentEvent::AssistantDelta("sum".into()),
                AgentEvent::AssistantDelta("mary".into()),
            ]
        );
    }

    #[tokio::test]
    async fn new_keeps_an_already_wired_event_sender() {
        // Subagents bind their bash tool to the PARENT's BackgroundTasks
        // (already wired to the parent's channel). Agent::new must not
        // retarget such a tool to the subagent's own channel, or the
        // completion would die with the subagent.
        struct PreWired;
        #[async_trait]
        impl Tool for PreWired {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "prewired".into(),
                    description: "already has a sender".into(),
                    parameters: json!({"type": "object"}),
                }
            }
            async fn execute(&self, _: Value) -> Result<String, String> {
                Ok("ok".into())
            }
            fn set_event_sender(&mut self, _: mpsc::UnboundedSender<AgentEvent>) {
                panic!("Agent::new must not retarget a pre-wired tool");
            }
            fn has_event_sender(&self) -> bool {
                true
            }
        }
        let model = ScriptedModel {
            replies: vec![AssistantMessage {
                content: Some("done".into()),
                tool_calls: vec![],
                reasoning: None,
            }],
            requests: Arc::new(Mutex::new(Vec::new())),
            delays: Default::default(),
        };
        let mut agent = Agent::new(Box::new(model), vec![Box::new(PreWired)]);
        assert_eq!(agent.run("go".into()).await.unwrap(), "done");
    }

    #[tokio::test]
    async fn records_and_clears_background_tasks_under_the_workspace() {
        // A background start is recorded on disk; its completion clears the
        // record, so only tasks that die WITH the process remain for the
        // next launch to report.
        let temp = tempfile::tempdir().unwrap();
        let model = ScriptedModel {
            replies: vec![
                AssistantMessage {
                    content: None,
                    tool_calls: vec![call(
                        "background-1",
                        "bash",
                        r#"{"command":"echo done","background":true}"#,
                    )],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("started".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("reacted".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
            ],
            requests: Arc::new(Mutex::new(Vec::new())),
            delays: Default::default(),
        };
        let mut agent = Agent::new(
            Box::new(model),
            vec![Box::new(ScriptedBackgroundTool { sender: None })],
        );
        agent.record_background_tasks_in(temp.path().to_path_buf(), "test");
        assert_eq!(agent.run("go".into()).await.unwrap(), "started");
        // Task recorded while in flight (its completion arrives 10ms after
        // start; the first run finished before that).
        let record = temp.path().join(".e-agent/sessions/test.background.jsonl");
        assert!(record.exists());
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        // The follow-up run drains the completion and clears the record.
        assert_eq!(agent.run("next".into()).await.unwrap(), "reacted");
        assert!(!record.exists());
    }
}
