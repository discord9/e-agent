use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use tokio::sync::mpsc;

/// Content of the synthetic error result inserted for every tool call left
/// unanswered by an interrupted turn (cancel, provider error, crash). Also
/// the legacy marker: sessions written before the `synthetic` flag existed
/// (commit 92159c7) recognized these placeholders by this literal text.
const INTERRUPTED: &str = "[turn interrupted before a tool result was produced]";

/// Insert a synthetic error result for every tool call left unanswered by an
/// interrupted turn (cancel, provider error, crash), so the derived context
/// always satisfies the provider's tool_call/tool-result pairing rule.
pub(crate) fn repair_tool_pairs(messages: Vec<Message>) -> Vec<Message> {
    fn flush(pending: &mut Vec<ToolCall>, out: &mut Vec<Message>) {
        for call in pending.drain(..) {
            out.push(Message::Tool {
                call_id: call.id,
                name: call.name,
                content: INTERRUPTED.into(),
                is_error: true,
                synthetic: true,
            });
        }
    }

    let mut out = Vec::with_capacity(messages.len());
    let mut pending: Vec<ToolCall> = Vec::new();
    for message in messages {
        match &message {
            Message::Tool {
                call_id,
                synthetic: true,
                ..
            } => {
                // Synthetic placeholder from an interrupted-turn snapshot
                // (e.g. captured in a compaction `retained` window). Skip it
                // without consuming the pending call: the real result may
                // land later (compaction race), and if it never does the
                // final flush re-synthesizes an equivalent placeholder.
                let _ = call_id;
            }
            Message::Tool { call_id, .. } => {
                if pending.iter().any(|call| call.id == *call_id) {
                    pending.retain(|call| &call.id != call_id);
                    out.push(message);
                }
                // Orphan tool result: no pending tool_call matches this
                // call_id (e.g. the same result was already answered by a
                // synthetic entry captured in a compaction snapshot, or the
                // call itself was compacted away). Dropping it keeps the
                // provider's tool_call/tool-result pairing rule intact.
                //
                // Ordering invariant: the real result for a tool_call always
                // lands inside the retained context window, so an orphan here
                // can only be a duplicate of a result that already paired —
                // never a first-time result. If a future change inserts
                // messages between a placeholder and its real result, this
                // drop would silently lose a real result; keep the
                // "real result within the retained window" invariant in mind.
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
        /// Synthetic interrupted-turn placeholder from a snapshot/compaction
        /// repair, never a real tool result. Used instead of matching the
        /// placeholder text so a real result with identical content is not
        /// mistaken for it.
        #[serde(default)]
        synthetic: bool,
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
    /// A prompt accepted while the runner is busy. Transient UI projection;
    /// kept in the in-memory event log for attach replay, but never persisted
    /// as a `SessionEntry`.
    PromptQueued(String),
    /// The runner consumed the oldest queued prompt. Transient UI projection.
    PromptConsumed,
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
    /// A turn failed. Recorded even when no frontend is attached.
    Error(String),
    /// Emitted on the turn boundary when a background task's completion is
    /// folded into the model context as a `[background task N completed]`
    /// message. Not part of the session event log.
    BackgroundCompleted {
        id: u64,
        output: String,
        label: Option<String>,
    },
    /// A structured background completion notice for TUI scrollback display.
    /// Unlike `BackgroundCompleted` (the transient arrival/drain signal),
    /// this is fanned out to observers and persisted as a
    /// `SessionEntry::BackgroundCompletion`. The TUI renders it once as a
    /// dim line; the model sees the full text via `context()`.
    BackgroundCompletionNotice {
        id: u64,
        output: String,
        label: Option<String>,
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
    /// A structured background completion entry. Persisted in the session
    /// log with the full output. The TUI renders a truncated preview; the
    /// model context sees the full text. Backwards-compatible: old
    /// `Notice` entries are read without guessing string prefixes.
    BackgroundCompletion {
        id: u64,
        output: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
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

pub(crate) struct RoundOutput {
    pub(crate) assistant: AssistantMessage,
    pub(crate) usage: Option<Usage>,
    pub(crate) produced_content_delta: bool,
}

pub(crate) struct CompactionOutput {
    pub(crate) entry: SessionEntry,
    pub(crate) summary: String,
    pub(crate) usage: Option<Usage>,
}

pub struct Agent {
    model: Box<dyn Model>,
    tools: Vec<Box<dyn Tool>>,
    history: Vec<SessionEntry>,
    event_handler: Option<Box<dyn FnMut(AgentEvent) + Send>>,
    max_tool_rounds: Option<usize>,
    background_receiver: mpsc::UnboundedReceiver<AgentEvent>,
    pending_background: VecDeque<(u64, String, Option<String>)>,
    subscriber: Option<mpsc::UnboundedSender<AgentEvent>>,
    /// Long-lived session sinks (e.g. a TUI view). Unlike
    /// `event_handler` and `subscriber` (per-turn), these survive across
    /// turns and receive every emitted event. Sinks are dropped once their
    /// session handle is gone.
    running_background: HashSet<u64>,
    /// Where in-flight background tasks are recorded (workspace root +
    /// session + the store that owns the record), so resuming the same
    /// session can report what died. None in tests.
    background_record: Option<crate::session_store::BackgroundRecord>,
    session_input_tokens: u64,
    session_output_tokens: u64,
    last_context_input: u64,
    /// Maximum context window in tokens. When set, triggers auto-compaction
    /// whenever `last_context_input` exceeds 80% of this value.
    context_window: Option<u64>,
    /// Set to true when auto-compact fires. Reset to false when
    /// record_usage reports context below 80% of the window.
    auto_compacted: bool,
    /// Workspace and server instructions prepended to every model call.
    /// Not persisted in sessions.
    context_prefix: Option<String>,
}

impl Agent {
    pub fn new(model: Box<dyn Model>, mut tools: Vec<Box<dyn Tool>>) -> Self {
        let (background_sender, background_receiver) = mpsc::unbounded_channel();
        for tool in &mut tools {
            // A tool may own an explicit sink (notably a Bash facade). Such
            // completions retain the session that spawned them.
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
            running_background: HashSet::new(),
            background_record: None,
            session_input_tokens: 0,
            session_output_tokens: 0,
            last_context_input: 0,
            context_window: None,
            auto_compacted: false,
            context_prefix: None,
        }
    }

    /// Extra system context prepended to every model call. Not persisted in
    /// sessions.
    pub fn set_context_prefix(&mut self, prefix: String) {
        self.context_prefix = Some(prefix);
    }

    /// Record in-flight background tasks under this workspace root + session
    /// through the given store, so resuming the same session later can
    /// report what died with this process.
    pub fn record_background_tasks_in(
        &mut self,
        root: std::path::PathBuf,
        session: &str,
        store: crate::session_store::SessionStore,
    ) {
        self.background_record = Some(crate::session_store::BackgroundRecord {
            root,
            session: session.to_owned(),
            store,
        });
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
        self.history = Self::migrate_legacy_placeholders(history);
    }

    /// Append a single entry to the history (e.g. a startup notice injected
    /// after resume).
    pub fn push_entry(&mut self, entry: SessionEntry) {
        self.history.push(entry);
    }

    /// One-time migration for sessions written before the `synthetic` flag
    /// existed (commit 92159c7). Back then the interrupted-turn placeholder
    /// was recognized only by its literal text, and placeholders captured in
    /// compaction `retained` snapshots persist WITHOUT the flag — they
    /// deserialize as `synthetic: false`, so `repair_tool_pairs` would
    /// consume them like real results and the real result arriving later
    /// would become a silently dropped orphan (the model would never see
    /// it). Mark legacy placeholders here at load time so the structured
    /// field alone decides from now on.
    fn migrate_legacy_placeholders(history: Vec<SessionEntry>) -> Vec<SessionEntry> {
        let mut history = history;
        for entry in &mut history {
            match entry {
                SessionEntry::Message { message } => Self::mark_legacy_placeholder(message),
                SessionEntry::Compaction { retained, .. } => {
                    for message in retained {
                        Self::mark_legacy_placeholder(message);
                    }
                }
                SessionEntry::Notice { .. } | SessionEntry::BackgroundCompletion { .. } => {}
            }
        }
        history
    }

    /// Flag a placeholder written before the `synthetic` field existed:
    /// exactly the interrupted-turn text, an error result, and no flag yet.
    /// A real result with identical text but `is_error: false` is left
    /// untouched.
    fn mark_legacy_placeholder(message: &mut Message) {
        if let Message::Tool {
            content,
            is_error: true,
            synthetic,
            ..
        } = message
            && !*synthetic
            && content == INTERRUPTED
        {
            *synthetic = true;
        }
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
                    // Structured background completions: same surface as
                    // before, but derived from the structured variant rather
                    // than a string-prefixed Notice.
                    SessionEntry::BackgroundCompletion { id, output, label } => {
                        let header =
                            match label.as_ref().map(|l| l.trim()).filter(|l| !l.is_empty()) {
                                Some(l) => format!("[background task {id} completed: {l}]"),
                                None => format!("[background task {id} completed]"),
                            };
                        Some(Message::User {
                            content: format!("{header}\n{output}"),
                        })
                    }
                }),
        );
        repair_tool_pairs(messages)
    }

    pub fn subscribe(&mut self, sender: mpsc::UnboundedSender<AgentEvent>) {
        self.subscriber = Some(sender);
    }

    pub fn background_task_ids(&self) -> &HashSet<u64> {
        &self.running_background
    }
    pub(crate) fn has_running_background(&self) -> bool {
        !self.running_background.is_empty()
    }

    /// Names of the registered tools (for tests and diagnostics).
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.iter().map(|tool| tool.spec().name).collect()
    }

    /// Wait for the next background task completion. Used by the TUI to
    /// wake an idle agent. The event is also queued for injection into the
    /// next model call.
    pub async fn next_background_completion(&mut self) -> Option<(u64, String, Option<String>)> {
        loop {
            match self.background_receiver.recv().await {
                Some(AgentEvent::BackgroundCompleted { id, output, label }) => {
                    self.pending_background
                        .push_back((id, output.clone(), label.clone()));
                    // No fanout here either: idle and mid-turn completions
                    // both land in the session log as a user message at the
                    // next turn boundary. The TUI prints this return value
                    // itself; fanning out would duplicate the line.
                    return Some((id, output, label));
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
        // Reset the auto-compact latch at the start of each new user turn so
        // a failed compaction doesn't permanently prevent future attempts.
        self.auto_compacted = false;
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
        let prepared = self.prepare_compaction().await?;
        let summary = prepared.summary;
        self.apply_entry(prepared.entry);
        self.apply_usage(prepared.usage, false);
        Ok(summary)
    }

    pub(crate) async fn prepare_compaction(&mut self) -> anyhow::Result<CompactionOutput> {
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
        // Skip the context prefix (System messages) — only compact if
        // there is actual conversation history (at least one assistant or
        // tool message) before the retained user turn.
        if !context[..split]
            .iter()
            .any(|msg| matches!(msg, Message::Assistant(_) | Message::Tool { .. }))
        {
            anyhow::bail!("nothing to compact");
        }
        let mut request = context[..split].to_vec();
        request.push(Message::User {
            content: "Summarize the earlier conversation. Preserve the user's goals, decisions made, files changed, and unfinished work. Be concise and use Chinese or English to match the conversation language.".into(),
        });
        let response = {
            let model = &mut self.model;
            let event_handler = &mut self.event_handler;
            let mut on_delta = |kind: ModelDeltaKind, delta: &str| {
                let event = match kind {
                    ModelDeltaKind::Content => AgentEvent::AssistantDelta(delta.into()),
                    ModelDeltaKind::Reasoning => AgentEvent::ReasoningDelta(delta.into()),
                };
                if let Some(handler) = event_handler {
                    handler(event.clone());
                }
            };
            model.complete(&request, &[], Some(&mut on_delta)).await?
        };
        let (response, usage) = response;
        if !response.tool_calls.is_empty() {
            anyhow::bail!("compaction response contains tool calls");
        }
        let summary = response
            .content
            .filter(|c| !c.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("compaction produced empty summary"))?;
        Ok(CompactionOutput {
            entry: SessionEntry::Compaction {
                summary: summary.clone(),
                retained: context[split..].to_vec(),
            },
            summary,
            usage,
        })
    }

    pub(crate) fn tool_specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|tool| tool.spec()).collect()
    }

    pub(crate) async fn execute_tool(&mut self, call: &ToolCall) -> Result<String, String> {
        Self::execute_on(&self.tools, call).await
    }

    pub(crate) fn apply_entry(&mut self, entry: SessionEntry) {
        self.history.push(entry);
    }

    pub(crate) fn apply_usage(&mut self, usage: Option<Usage>, refresh_context: bool) {
        self.record_usage(usage, refresh_context);
    }

    pub(crate) fn emit_event(&mut self, event: AgentEvent) {
        self.emit(event);
    }

    pub(crate) fn after_tool_entry(&mut self, call: &ToolCall, result: &Result<String, String>) {
        if result.is_ok()
            && call.name == "bash"
            && is_background_call(call)
            && let Some(id) = started_task_id(result.as_deref().unwrap_or_default())
        {
            self.running_background.insert(id);
            if let Some(record) = &self.background_record {
                let command = serde_json::from_str::<Value>(&call.arguments)
                    .ok()
                    .and_then(|args| args["command"].as_str().map(str::to_owned))
                    .unwrap_or_else(|| call.arguments.clone());
                let label = preview(&command, 100);
                record.store.record_background_start(
                    &record.root,
                    &record.session,
                    id,
                    &label,
                    None,
                );
            }
        }
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
            // Reset auto-compacted flag when context drops below 80% of the
            // window (compaction succeeded and usage decreased).
            if self.auto_compacted
                && let Some(window) = self.context_window
                && window > 0
                && (self.last_context_input as u128) * 100 < (window as u128) * 80
            {
                self.auto_compacted = false;
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

    pub(crate) async fn complete_round(
        &mut self,
        specs: &[ToolSpec],
    ) -> anyhow::Result<RoundOutput> {
        let mut produced_content_delta = false;
        let context = self.context();
        let model = &mut self.model;
        let event_handler = &mut self.event_handler;
        let mut on_delta = |kind: ModelDeltaKind, delta: &str| {
            if kind == ModelDeltaKind::Content {
                produced_content_delta = true;
            }
            let event = match kind {
                ModelDeltaKind::Content => AgentEvent::AssistantDelta(delta.into()),
                ModelDeltaKind::Reasoning => AgentEvent::ReasoningDelta(delta.into()),
            };
            if let Some(handler) = event_handler {
                handler(event.clone());
            }
        };
        let (assistant, usage) = model.complete(&context, specs, Some(&mut on_delta)).await?;
        Ok(RoundOutput {
            assistant,
            usage,
            produced_content_delta,
        })
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
            let round = self.complete_round(specs).await?;
            let RoundOutput {
                assistant,
                usage,
                produced_content_delta: produced_delta,
            } = round;
            self.record_usage(usage, true);
            // Auto-compact when usage exceeds 80% of the configured context window.
            if let Some(window) = self.context_window
                && window > 0
                && !self.auto_compacted
                && (self.last_context_input as u128) * 100 >= (window as u128) * 80
            {
                self.auto_compacted = true;
                self.emit(AgentEvent::Notice("──── auto-compacting… ────".into()));
                if let Err(error) = self.compact().await {
                    self.auto_compacted = false;
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
                self.after_tool_entry(call, &result);
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
                    synthetic: false,
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
    }

    fn inject_pending_background(&mut self) {
        while !self.pending_background.is_empty() {
            let entry = self.peek_background_entry().expect("pending entry");
            self.apply_entry(entry);
            self.ack_background_entry();
        }
    }

    pub(crate) fn drain_background_ready(&mut self) {
        while let Ok(AgentEvent::BackgroundCompleted { id, output, label }) =
            self.background_receiver.try_recv()
        {
            self.pending_background
                .push_back((id, output.clone(), label.clone()));
            if let Some(subscriber) = &self.subscriber {
                let _ = subscriber.send(AgentEvent::BackgroundCompleted { id, output, label });
            }
        }
    }

    pub(crate) async fn wait_background_ready(&mut self) -> bool {
        self.next_background_completion().await.is_some()
    }

    pub(crate) fn peek_background_entry(&self) -> Option<SessionEntry> {
        self.pending_background.front().map(|(id, output, label)| {
            SessionEntry::BackgroundCompletion {
                id: *id,
                output: output.clone(),
                label: label.clone(),
            }
        })
    }

    pub(crate) fn ack_background_entry(&mut self) {
        if let Some((id, _output, _label)) = self.pending_background.pop_front() {
            self.running_background.remove(&id);
            if let Some(record) = &self.background_record {
                record
                    .store
                    .clear_background_task(&record.root, &record.session, id);
            }
        }
    }

    pub(crate) fn take_auto_compact_request(&mut self) -> bool {
        let requested = self.context_window.is_some_and(|window| {
            window > 0
                && !self.auto_compacted
                && (self.last_context_input as u128) * 100 >= (window as u128) * 80
        });
        if requested {
            self.auto_compacted = true;
        }
        requested
    }

    pub(crate) fn reset_auto_compact_request(&mut self) {
        self.auto_compacted = false;
    }

    fn drain_background(&mut self) {
        self.drain_background_ready();
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
    let total: Vec<char> = text.chars().collect();
    if total.len() <= max_chars || max_chars <= 1 {
        // max_chars 0 → empty; max_chars 1 → a single ellipsis only
        if max_chars == 0 {
            return String::new();
        }
        if total.len() <= max_chars {
            return text.to_owned();
        }
        // max_chars == 1 and text longer than 1: show just "…"
        return "\u{2026}".to_owned();
    }
    // Keep a 2:1 head-to-tail ratio within a budget of max_chars (including "…").
    let marker_len = 1; // "…"
    let available = max_chars - marker_len;
    let head_count = (available * 2 / 3).max(1);
    let tail_count = available - head_count;
    let head: String = total[..head_count].iter().collect();
    let tail: String = total[total.len() - tail_count..].iter().collect();
    format!("{head}\u{2026}{tail}")
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;

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
                    label: None,
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

    #[test]
    fn repair_tool_pairs_drops_orphan_results_after_a_synthetic_answer() {
        // Compaction captures a synthetic interrupted-result for call-1,
        // then the real result lands later (duplicate call_id). The second
        // Tool message has no pending tool_call and must be dropped.
        let messages = vec![
            Message::User {
                content: "u".into(),
            },
            Message::Assistant(AssistantMessage {
                content: None,
                tool_calls: vec![call("call-1", "bash", r#"{"cmd":"x"}"#)],
                reasoning: None,
            }),
            Message::Tool {
                call_id: "call-1".into(),
                name: "bash".into(),
                content: "[turn interrupted before a tool result was produced]".into(),
                is_error: true,
                synthetic: true,
            },
            Message::Tool {
                call_id: "call-1".into(),
                name: "bash".into(),
                content: "real result".into(),
                is_error: false,
                synthetic: false,
            },
        ];
        let repaired = repair_tool_pairs(messages);
        assert_eq!(repaired.len(), 3);
        // The synthetic placeholder is skipped so the real result can claim
        // the pending call: output = [User, Assistant(call-1), Tool(real)].
        assert!(matches!(
            &repaired[2],
            Message::Tool { call_id, content, is_error: false, synthetic: false, .. }
                if call_id == "call-1" && content == "real result"
        ));
        assert!(!repaired.iter().any(|m| matches!(
            m,
            Message::Tool {
                synthetic: true,
                ..
            }
        )));
    }

    #[test]
    fn repair_tool_pairs_synthesizes_missing_results_in_order() {
        let messages = vec![
            Message::User {
                content: "u".into(),
            },
            Message::Assistant(AssistantMessage {
                content: None,
                tool_calls: vec![call("call-1", "bash", r#"{}"#)],
                reasoning: None,
            }),
            Message::User {
                content: "next".into(),
            },
        ];
        let repaired = repair_tool_pairs(messages);
        assert!(repaired.iter().any(|message| matches!(
            message,
            Message::Tool { call_id, is_error: true, synthetic: true, .. }
                if call_id == "call-1" && message_tool_content(message) == "[turn interrupted before a tool result was produced]"
        )));
        // The synthetic result must precede the following user message.
        let index = repaired
            .iter()
            .position(|m| matches!(m, Message::Tool { call_id, .. } if call_id == "call-1"))
            .unwrap();
        assert!(matches!(&repaired[index + 1], Message::User { .. }));
    }

    #[test]
    fn repair_tool_pairs_keeps_real_result_matching_placeholder_text() {
        // A real tool result whose content happens to equal the interrupted
        // placeholder text must still pair normally: it is not skipped and
        // no placeholder is synthesized on top of it.
        let messages = vec![
            Message::User {
                content: "u".into(),
            },
            Message::Assistant(AssistantMessage {
                content: None,
                tool_calls: vec![call("call-1", "bash", r#"{}"#)],
                reasoning: None,
            }),
            Message::Tool {
                call_id: "call-1".into(),
                name: "bash".into(),
                content: "[turn interrupted before a tool result was produced]".into(),
                is_error: false,
                synthetic: false,
            },
        ];
        let repaired = repair_tool_pairs(messages);
        assert_eq!(repaired.len(), 3);
        assert!(matches!(
            &repaired[2],
            Message::Tool { call_id, content, is_error: false, synthetic: false, .. }
                if call_id == "call-1"
                    && content == "[turn interrupted before a tool result was produced]"
        ));
        // Only that one real result exists: no placeholder was flushed.
        assert_eq!(
            repaired
                .iter()
                .filter(|m| matches!(
                    m,
                    Message::Tool { content, .. }
                        if *content == "[turn interrupted before a tool result was produced]"
                ))
                .count(),
            1
        );
        assert!(!repaired.iter().any(|m| matches!(
            m,
            Message::Tool {
                synthetic: true,
                ..
            }
        )));
    }

    #[test]
    fn restore_history_migrates_legacy_interrupted_placeholders() {
        // A session file written before commit 92159c7: its interrupted-turn
        // placeholders — both a plain message entry and one inside a
        // compaction `retained` snapshot — carry no `synthetic` field and
        // deserialize as synthetic: false. restore_history must flag them so
        // repair_tool_pairs skips them instead of consuming them like real
        // results.
        let legacy_message = serde_json::json!({
            "type": "message",
            "message": {
                "Tool": {
                    "call_id": "call-1",
                    "name": "bash",
                    "content": INTERRUPTED,
                    "is_error": true,
                }
            }
        });
        let legacy_compaction = serde_json::json!({
            "type": "compaction",
            "summary": "old summary",
            "retained": [
                {
                    "User": { "content": "current question" }
                },
                {
                    "Assistant": {
                        "content": null,
                        "tool_calls": [
                            {
                                "id": "call-2",
                                "name": "bash",
                                "arguments": "{\"command\":\"make\"}"
                            }
                        ]
                    }
                },
                {
                    "Tool": {
                        "call_id": "call-2",
                        "name": "bash",
                        "content": INTERRUPTED,
                        "is_error": true,
                    }
                }
            ]
        });
        let entries: Vec<SessionEntry> = vec![
            serde_json::from_value(legacy_message).unwrap(),
            serde_json::from_value(legacy_compaction).unwrap(),
        ];
        let mut agent = Agent::new(
            Box::new(ScriptedModel {
                replies: vec![],
                requests: Arc::new(Mutex::new(Vec::new())),
                delays: Default::default(),
            }),
            vec![],
        );
        agent.restore_history(entries);
        assert!(matches!(
            &agent.history()[0],
            SessionEntry::Message {
                message: Message::Tool {
                    content,
                    is_error: true,
                    synthetic: true,
                    ..
                }
            } if content == INTERRUPTED
        ));
        let SessionEntry::Compaction { retained, .. } = &agent.history()[1] else {
            panic!("expected compaction entry");
        };
        // The assistant turn is untouched; the legacy placeholder is flagged.
        assert!(matches!(&retained[1], Message::Assistant(_)));
        assert!(matches!(
            &retained[2],
            Message::Tool {
                content,
                is_error: true,
                synthetic: true,
                ..
            } if content == INTERRUPTED
        ));
    }

    #[test]
    fn restore_history_migrates_legacy_placeholders_end_to_end() {
        // Full regression for the legacy gap: a pre-92159c7 session whose
        // compaction `retained` snapshot holds the assistant tool_call plus
        // the text-only interrupted placeholder, with the REAL result
        // persisted after the Compaction entry. After the load-time
        // migration, context() must skip the placeholder and pair the real
        // result — the model sees the real result, nothing is orphaned.
        let legacy_compaction = serde_json::json!({
            "type": "compaction",
            "summary": "old summary",
            "retained": [
                {
                    "User": { "content": "current question" }
                },
                {
                    "Assistant": {
                        "content": null,
                        "tool_calls": [
                            {
                                "id": "call-1",
                                "name": "bash",
                                "arguments": "{\"command\":\"make\"}"
                            }
                        ]
                    }
                },
                {
                    "Tool": {
                        "call_id": "call-1",
                        "name": "bash",
                        "content": INTERRUPTED,
                        "is_error": true,
                    }
                }
            ]
        });
        let mut agent = Agent::new(
            Box::new(ScriptedModel {
                replies: vec![],
                requests: Arc::new(Mutex::new(Vec::new())),
                delays: Default::default(),
            }),
            vec![],
        );
        agent.restore_history(vec![
            serde_json::from_value(legacy_compaction).unwrap(),
            Message::Tool {
                call_id: "call-1".into(),
                name: "bash".into(),
                content: "real result".into(),
                is_error: false,
                synthetic: false,
            }
            .into(),
        ]);
        let context = agent.context();
        assert_eq!(context.len(), 4);
        assert!(matches!(
            &context[0],
            Message::User { content }
                if content == "[compacted summary of earlier conversation]\nold summary"
        ));
        assert!(matches!(&context[1], Message::User { .. }));
        assert!(matches!(
            &context[2],
            Message::Assistant(message)
                if message.tool_calls == vec![call("call-1", "bash", r#"{"command":"make"}"#)]
        ));
        assert!(matches!(
            &context[3],
            Message::Tool { call_id, content, is_error: false, synthetic: false, .. }
                if call_id == "call-1" && content == "real result"
        ));
        // No placeholder reaches the provider, and nothing is left orphaned:
        // re-running the repair over the derived context is a fixed point.
        assert!(!context.iter().any(|m| matches!(
            m,
            Message::Tool {
                synthetic: true,
                ..
            }
        )));
        assert_eq!(repair_tool_pairs(context.clone()), context);
    }

    #[test]
    fn context_pairs_real_result_across_a_compaction_snapshot() {
        // End-to-end (c): a compaction `retained` snapshot holds the
        // assistant tool_call plus its synthetic interrupted placeholder
        // (post-92159c7 shape); the real tool result landed in the history
        // AFTER the Compaction entry. context() must skip the placeholder
        // and pair the real result with its tool_call — no orphan, no
        // unpaired call (the 400-class malformation).
        let mut agent = Agent::new(
            Box::new(ScriptedModel {
                replies: vec![],
                requests: Arc::new(Mutex::new(Vec::new())),
                delays: Default::default(),
            }),
            vec![],
        );
        agent.restore_history(vec![
            SessionEntry::Compaction {
                summary: "summary text".into(),
                retained: vec![
                    Message::User {
                        content: "current question".into(),
                    },
                    Message::Assistant(AssistantMessage {
                        content: None,
                        tool_calls: vec![call("call-1", "bash", r#"{"command":"make"}"#)],
                        reasoning: None,
                    }),
                    Message::Tool {
                        call_id: "call-1".into(),
                        name: "bash".into(),
                        content: INTERRUPTED.into(),
                        is_error: true,
                        synthetic: true,
                    },
                ],
            },
            Message::Tool {
                call_id: "call-1".into(),
                name: "bash".into(),
                content: "real result".into(),
                is_error: false,
                synthetic: false,
            }
            .into(),
        ]);
        let context = agent.context();
        assert_eq!(context.len(), 4);
        assert!(matches!(
            &context[0],
            Message::User { content }
                if content == "[compacted summary of earlier conversation]\nsummary text"
        ));
        assert!(matches!(&context[1], Message::User { .. }));
        assert!(matches!(
            &context[2],
            Message::Assistant(message)
                if message.tool_calls == vec![call("call-1", "bash", r#"{"command":"make"}"#)]
        ));
        assert!(matches!(
            &context[3],
            Message::Tool { call_id, content, is_error: false, synthetic: false, .. }
                if call_id == "call-1" && content == "real result"
        ));
        assert!(!context.iter().any(|m| matches!(
            m,
            Message::Tool {
                synthetic: true,
                ..
            }
        )));
        assert_eq!(repair_tool_pairs(context.clone()), context);
    }

    fn message_tool_content(message: &Message) -> &str {
        match message {
            Message::Tool { content, .. } => content,
            _ => "",
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
            Message::Tool { call_id, name, content, is_error: false, .. }
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
        // The completion is injected as a BackgroundCompletion, which
        // context() surfaces as a Message::User so the model sees it.
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
                synthetic: false,
            },
            Message::Tool {
                call_id: "call-3".into(),
                name: "bash".into(),
                content: "still building".into(),
                is_error: false,
                synthetic: false,
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
                synthetic: false,
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
                synthetic: false,
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

    #[tokio::test]
    async fn new_keeps_an_already_wired_event_sender() {
        // Tools with an explicit event sender retain it when Agent::new
        // attaches its default session sink.
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
        agent.record_background_tasks_in(
            temp.path().to_path_buf(),
            "test",
            crate::session_store::SessionStore::Jsonl,
        );
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

    // ── Preview tests (middle-ellipsis) ──────────────────────────────────

    #[test]
    fn preview_short_text_unchanged() {
        assert_eq!(preview("hello", 10), "hello");
        assert_eq!(preview("hi", 2), "hi");
        assert_eq!(preview("", 5), "");
    }

    #[test]
    fn preview_exact_fit() {
        assert_eq!(preview("abcde", 5), "abcde");
    }

    #[test]
    fn preview_zero_or_one() {
        assert_eq!(preview("hello", 0), "");
        assert_eq!(preview("h", 1), "h");
        assert_eq!(preview("hello", 1), "\u{2026}");
    }

    #[test]
    fn preview_max_2() {
        let r = preview("abcdef", 2);
        assert_eq!(r.chars().count(), 2);
        assert!(r.contains('\u{2026}'));
    }

    #[test]
    fn preview_middle_ellipsis_ascii() {
        let r = preview("abcdefghijklmno", 10);
        assert_eq!(r.chars().count(), 10);
        assert!(r.contains('\u{2026}'));
        // 2:1 head:tail => head=6, tail=3 (available=9)
        assert!(r.starts_with("abcdef"), "head preserved, got {r:?}");
        assert!(r.ends_with("mno"), "tail preserved, got {r:?}");
    }

    #[test]
    fn preview_middle_ellipsis_cjk() {
        let text = "你好世界数据驱动开发";
        let r = preview(text, 8);
        assert_eq!(r.chars().count(), 8);
        assert!(r.contains('\u{2026}'));
        // head=5 chars, tail=2 chars (available=7, 2:1 ratio)
        assert!(r.starts_with("你好世界"), "CJK head, got {r:?}");
        assert!(r.ends_with("开发"), "CJK tail, got {r:?}");
    }

    #[test]
    fn preview_middle_ellipsis_emoji() {
        let text = "a😊b😊c😊d😊e😊f😊g";
        let r = preview(text, 8);
        assert_eq!(r.chars().count(), 8);
        assert!(
            r.contains('\u{2026}'),
            "emoji preview has ellipsis, got {r:?}"
        );
        // char-count respects Unicode, not bytes
    }

    #[test]
    fn preview_char_count_never_exceeds_max() {
        for max in [3usize, 5, 10, 50, 100] {
            let text = "a".repeat(max * 2);
            let r = preview(&text, max);
            assert!(
                r.chars().count() <= max,
                "max={max}: actual {} > {max}, result: {r:?}",
                r.chars().count()
            );
        }
    }

    // ── BackgroundCompletion structured entry tests ──────────────────────

    #[test]
    fn background_completion_entry_serde_old_and_new() {
        // Old JSON without label → deserializes with label: None
        let old_json = r#"{"type":"background_completion","id":42,"output":"done"}"#;
        let deserialized: SessionEntry = serde_json::from_str(old_json).unwrap();
        assert!(
            matches!(
                deserialized,
                SessionEntry::BackgroundCompletion {
                    id: 42,
                    label: None,
                    ..
                }
            ),
            "old payload must have label=None, got {deserialized:?}"
        );
        // Roundtrip with a label
        let entry = SessionEntry::BackgroundCompletion {
            id: 43,
            output: "done".into(),
            label: Some("build project".into()),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(
            json.contains(r#""label":"build project""#),
            "label must be present: {json}"
        );
        let deserialized: SessionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, entry);
        // Roundtrip with None label (serialized without label field)
        let entry_none = SessionEntry::BackgroundCompletion {
            id: 44,
            output: "done".into(),
            label: None,
        };
        let json_none = serde_json::to_string(&entry_none).unwrap();
        assert!(
            !json_none.contains("label"),
            "None label must be skipped: {json_none}"
        );
        let deserialized_none: SessionEntry = serde_json::from_str(&json_none).unwrap();
        assert_eq!(deserialized_none, entry_none);
    }

    #[test]
    fn context_formats_background_completion_with_label_variants() {
        // Verify context() output for label=Some("build"), whitespace, and None.
        let cases: &[(&str, Option<&str>, &str)] = &[
            (
                "build",
                Some("build"),
                "[background task 7 completed: build]",
            ),
            ("", None, "[background task 7 completed]"),
            ("  ", None, "[background task 7 completed]"),
        ];
        for (_name, label_val, expected_header) in cases {
            let label = label_val.map(|s| s.to_string());
            let mut agent = Agent::new(
                Box::new(ScriptedModel {
                    replies: vec![],
                    requests: Arc::new(Mutex::new(Vec::new())),
                    delays: Default::default(),
                }),
                vec![],
            );
            agent.restore_history(vec![SessionEntry::BackgroundCompletion {
                id: 7,
                output: "full output text\nwith multiple\nlines".into(),
                label,
            }]);
            let msgs = agent.context();
            assert_eq!(msgs.len(), 1);
            let expected = format!("{expected_header}\nfull output text\nwith multiple\nlines");
            assert!(
                matches!(&msgs[0], Message::User { content } if content == &expected),
                "label={label_val:?}: expected {expected:?}, got {:?}",
                match &msgs[0] {
                    Message::User { content } => content,
                    _ => "(wrong variant)",
                }
            );
        }
    }

    #[test]
    fn background_completion_and_notice_coexist_in_context() {
        // Old Notice entries must still work alongside new BackgroundCompletion.
        let mut agent = Agent::new(
            Box::new(ScriptedModel {
                replies: vec![],
                requests: Arc::new(Mutex::new(Vec::new())),
                delays: Default::default(),
            }),
            vec![],
        );
        agent.restore_history(vec![
            SessionEntry::Notice {
                text: "[background task 1 completed]\nold style".into(),
            },
            SessionEntry::BackgroundCompletion {
                id: 2,
                output: "new style".into(),
                label: None,
            },
        ]);
        let msgs = agent.context();
        assert_eq!(msgs.len(), 2);
        assert!(matches!(
            &msgs[0],
            Message::User { content } if content.contains("old style")
        ));
        assert!(matches!(
            &msgs[1],
            Message::User { content } if content == "[background task 2 completed]\nnew style"
        ));
    }
}
