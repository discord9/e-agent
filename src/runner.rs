//! Durable, single-writer session runner built on the Agent step API.

use crate::{
    agent::{
        Agent, AgentEvent, CompactionOutput, ImagePart, Message, Model,
        POLL_GUARD_TERMINATION_NOTICE, RoundOutput, SessionEntry, ToolCall, ToolOutput, ToolSpec,
        is_poll_guard_terminate, tool_error_content,
    },
    session_store::{LocatedKey, SessionStore},
};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::{
    sync::{broadcast, mpsc, watch},
    task::JoinHandle,
};

const EVENT_CAPACITY: usize = 256;

/// Outcome of polling an in-flight operation against the command channel.
/// A `Cancel` command is a *release*: the in-flight future is dropped
/// (preempted) so queued messages are processed immediately — it never
/// terminates the session by itself.
enum WaitOutcome<T> {
    Completed(T),
    Released,
    Closed,
}

struct WaitResult<T> {
    outcome: WaitOutcome<T>,
    pending: Vec<SessionCommand>,
}

async fn await_round(
    agent: &mut Agent,
    specs: &[ToolSpec],
    commands: &mut mpsc::UnboundedReceiver<SessionCommand>,
) -> WaitResult<anyhow::Result<RoundOutput>> {
    let mut operation = Box::pin(agent.complete_round(specs));
    wait_for_operation(&mut operation, commands).await
}

async fn await_compaction(
    agent: &mut Agent,
    commands: &mut mpsc::UnboundedReceiver<SessionCommand>,
) -> WaitResult<anyhow::Result<CompactionOutput>> {
    let mut operation = Box::pin(agent.prepare_compaction());
    wait_for_operation(&mut operation, commands).await
}

async fn await_tool(
    agent: &mut Agent,
    call: &ToolCall,
    commands: &mut mpsc::UnboundedReceiver<SessionCommand>,
) -> WaitResult<Result<crate::agent::ToolOutput, String>> {
    let mut operation = Box::pin(async move { agent.execute_tool(call).await });
    wait_for_operation(&mut operation, commands).await
}

async fn wait_for_operation<F, T>(
    operation: &mut std::pin::Pin<Box<F>>,
    commands: &mut mpsc::UnboundedReceiver<SessionCommand>,
) -> WaitResult<T>
where
    F: std::future::Future<Output = T>,
{
    let mut pending = Vec::new();
    loop {
        tokio::select! {
            biased;
            value = operation.as_mut() => {
                // Drain any command that arrived in the same scheduling turn
                // as the completion: the biased select polls the operation
                // branch first, so a command can still be sitting in the
                // channel even though the operation is already ready.
                // Callers decide what to do with them (the runner must apply
                // a SwitchModel before interpreting a tool result, so the
                // vision guard sees the *new* model).
                while let Ok(command) = commands.try_recv() {
                    pending.push(command);
                }
                return WaitResult { outcome: WaitOutcome::Completed(value), pending };
            }
            command = commands.recv() => match command {
                Some(SessionCommand::Cancel) => {
                    return WaitResult { outcome: WaitOutcome::Released, pending };
                }
                Some(command) => pending.push(command),
                None => return WaitResult { outcome: WaitOutcome::Closed, pending },
            }
        }
    }
}

/// Steering commands accepted by a session's command channel. Not
/// `Clone`/`Debug`/`PartialEq`/`Eq` because `SwitchModel` carries a
/// `Box<dyn Model>`; tests compare via `matches!` instead.
pub enum SessionCommand {
    Prompt(String),
    /// A prompt with an image attached (REPL `/image <path>` entrance).
    PromptWithImage {
        text: String,
        image: ImagePart,
    },
    Cancel,
    Compact,
    /// Runtime model switch (web/TUI `/model <profile>`): the caller
    /// resolves the profile to a concrete model and hands it over; the
    /// runner only installs it on the agent.
    SwitchModel(Box<dyn Model>),
    /// Human-issued goal mutation (`/goal` commands, web API). The model
    /// never creates goals; its `update_goal` tool is intercepted by the
    /// runner with the same transition rules under an id + revision CAS.
    Goal(GoalCommand),
}

/// Human goal operations (creation is human-only; the model's
/// `update_goal` tool covers the rest with explicit id/revision).
#[derive(Clone, Debug)]
pub enum GoalCommand {
    /// Create the first revision (`/goal set …`, `POST /api/…/goal`).
    /// Rejected by the runner while a non-completed goal exists.
    Create {
        objective: String,
        success_criteria: Vec<String>,
    },
    /// An action against the CURRENT goal (human commands carry no id).
    Action(crate::agent::GoalAction),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdlePolicy {
    WaitForInput,
    FinishWhenIdle,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionStatus {
    Idle,
    Busy,
    Compacting,
    Finished(SessionResult),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionResult {
    Completed(Option<String>),
    Failed(String),
    Cancelled,
    Closed,
}

struct Shared {
    log: Vec<AgentEvent>,
    events: broadcast::Sender<AgentEvent>,
    status: watch::Sender<SessionStatus>,
    compaction_streaming: bool,
    commands_open: bool,
    /// Latest goal snapshot, mirrored from the runner for UI reads
    /// (REPL `/goal`, TUI GoalBar, web `GET /api/sessions/{id}/goal`).
    goal: Option<crate::agent::GoalSnapshot>,
}
impl Shared {
    fn emit(&mut self, event: AgentEvent) {
        self.log.push(event.clone());
        let _ = self.events.send(event);
    }

    fn emit_agent(&mut self, event: AgentEvent) {
        if self.compaction_streaming
            && matches!(
                event,
                AgentEvent::AssistantDelta(_) | AgentEvent::ReasoningDelta(_)
            )
        {
            self.emit_transient(event);
        } else {
            self.emit(event);
        }
    }

    fn emit_transient(&self, event: AgentEvent) {
        let _ = self.events.send(event);
    }
}

/// Cloneable frontend end. Dropping every handle closes the runner command channel.
#[derive(Clone)]
pub struct SessionHandle {
    shared: Arc<Mutex<Shared>>,
    commands: mpsc::UnboundedSender<SessionCommand>,
}
impl SessionHandle {
    pub fn prompt(&self, prompt: impl Into<String>) {
        self.prompt_inner(prompt.into(), None);
    }

    /// Queue a prompt with an image attached; the image rides along as a
    /// reference on the resulting `Message::User`.
    pub fn prompt_with_image(&self, prompt: impl Into<String>, image: ImagePart) {
        self.prompt_inner(prompt.into(), Some(image));
    }

    fn prompt_inner(&self, prompt: String, image: Option<ImagePart>) {
        let mut shared = self.shared.lock().unwrap();
        if !shared.commands_open || self.commands.is_closed() {
            return;
        }
        let command = match image {
            Some(image) => SessionCommand::PromptWithImage {
                text: prompt.clone(),
                image,
            },
            None => SessionCommand::Prompt(prompt.clone()),
        };
        if self.commands.send(command).is_ok() {
            if matches!(
                *shared.status.borrow(),
                SessionStatus::Busy | SessionStatus::Compacting
            ) {
                shared.emit(AgentEvent::PromptQueued(prompt));
            }
        } else {
            shared.commands_open = false;
        }
    }
    /// Cancel == release: sends `SessionCommand::Cancel` through the command
    /// channel, consumed at the next operation boundary (round, tool,
    /// compaction, or idle). It preempts the in-flight operation but does
    /// NOT terminate the session. With prompts queued, the outer loop drains
    /// the batch into a fresh turn that ends naturally; with none queued, a
    /// FinishWhenIdle runner finalizes `Cancelled` right here, and a
    /// WaitForInput runner returns to Idle and stays usable.
    ///
    /// To hard-terminate a session use `DELETE /api/sessions/{id}` or the
    /// tasks-panel cancel (which aborts a subagent through its parent's
    /// background-task registry); `cancel` never ends the session.
    pub fn cancel(&self) {
        let mut shared = self.shared.lock().unwrap();
        if shared.commands_open
            && !self.commands.is_closed()
            && self.commands.send(SessionCommand::Cancel).is_err()
        {
            shared.commands_open = false;
        }
    }
    pub fn compact(&self) {
        let mut shared = self.shared.lock().unwrap();
        if shared.commands_open
            && !self.commands.is_closed()
            && self.commands.send(SessionCommand::Compact).is_err()
        {
            shared.commands_open = false;
        }
    }
    /// Switch the session's model at runtime. The caller resolves the
    /// profile (web `/model`, TUI `/model`); the runner installs the new
    /// model on the agent from its next call on.
    pub fn switch_model(&self, model: Box<dyn Model>) {
        let mut shared = self.shared.lock().unwrap();
        if shared.commands_open
            && !self.commands.is_closed()
            && self
                .commands
                .send(SessionCommand::SwitchModel(model))
                .is_err()
        {
            shared.commands_open = false;
        }
    }
    /// Queue a human goal mutation (create / pause / resume / clear). The
    /// runner applies it at the next safe point and persists a
    /// `GoalUpdated` entry; failures surface as `AgentEvent::Error`.
    /// Returns `true` when the command was accepted for queuing, `false`
    /// when the command channel is closed (session finished or dropped) —
    /// callers must never report a hollow success in that case.
    pub fn goal_command(&self, command: GoalCommand) -> bool {
        let mut shared = self.shared.lock().unwrap();
        if shared.commands_open && !self.commands.is_closed() {
            if self.commands.send(SessionCommand::Goal(command)).is_err() {
                shared.commands_open = false;
                return false;
            }
            return true;
        }
        false
    }
    /// The latest committed goal snapshot (`None` = none/cleared). Pure
    /// read for UIs; mutations go through [`Self::goal_command`] or the
    /// model's `update_goal` tool.
    pub fn goal(&self) -> Option<crate::agent::GoalSnapshot> {
        self.shared.lock().unwrap().goal.clone()
    }
    pub fn snapshot(&self) -> Vec<AgentEvent> {
        self.shared.lock().unwrap().log.clone()
    }
    /// Atomically obtains replay, live subscription, and status snapshot (no attach gap).
    pub fn attach(
        &self,
    ) -> (
        Vec<AgentEvent>,
        broadcast::Receiver<AgentEvent>,
        watch::Receiver<SessionStatus>,
    ) {
        let shared = self.shared.lock().unwrap();
        (
            shared.log.clone(),
            shared.events.subscribe(),
            shared.status.subscribe(),
        )
    }
    pub fn status(&self) -> watch::Receiver<SessionStatus> {
        self.shared.lock().unwrap().status.subscribe()
    }
}

#[cfg(test)]
pub(crate) struct TestSessionEmitter {
    shared: Arc<Mutex<Shared>>,
}
#[cfg(test)]
impl TestSessionEmitter {
    pub(crate) fn emit(&self, event: AgentEvent) {
        self.shared.lock().unwrap().emit(event);
    }
    /// Test-only: force the runner's status watch to a specific value so
    /// tests can simulate a Busy/Compacting/Finished subagent handle
    /// without a live runner task.
    pub(crate) fn set_status(&self, status: SessionStatus) {
        self.shared.lock().unwrap().status.send_replace(status);
    }
}
#[cfg(test)]
pub(crate) fn session_test_channel() -> (
    SessionHandle,
    TestSessionEmitter,
    mpsc::UnboundedReceiver<SessionCommand>,
) {
    let (events, _) = broadcast::channel(EVENT_CAPACITY);
    let (status, _) = watch::channel(SessionStatus::Idle);
    let shared = Arc::new(Mutex::new(Shared {
        log: Vec::new(),
        events,
        status,
        compaction_streaming: false,
        commands_open: true,
        goal: None,
    }));
    let (commands, receiver) = mpsc::unbounded_channel();
    (
        SessionHandle {
            shared: shared.clone(),
            commands,
        },
        TestSessionEmitter { shared },
        receiver,
    )
}

pub struct SessionTask {
    task: Option<JoinHandle<()>>,
}
impl SessionTask {
    /// Abort the underlying runner task (idempotent; the `Drop` impl does
    /// the same on normal exit paths).
    pub fn abort(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }

    pub fn abort_handle(&self) -> tokio::task::AbortHandle {
        self.task
            .as_ref()
            .expect("task already joined")
            .abort_handle()
    }
}
impl Drop for SessionTask {
    fn drop(&mut self) {
        self.abort();
    }
}
impl SessionTask {
    pub async fn join(mut self) -> Result<(), tokio::task::JoinError> {
        let result = self.task.as_mut().expect("task already joined").await;
        self.task = None;
        result
    }
}

#[cfg(test)]
impl SessionTask {
    /// Test-only constructor: wrap a bare join handle so server tests can
    /// build a `LiveSession` without running a real runner.
    pub(crate) fn from_join_handle(task: JoinHandle<()>) -> Self {
        Self { task: Some(task) }
    }
}

#[derive(Clone, Copy)]
enum CompactionSource {
    Manual,
    Auto,
}
impl CompactionSource {
    fn prefix(self) -> &'static str {
        match self {
            Self::Manual => "",
            Self::Auto => "auto-",
        }
    }
    fn resume_status(self) -> SessionStatus {
        match self {
            Self::Manual => SessionStatus::Idle,
            Self::Auto => SessionStatus::Busy,
        }
    }
}

/// Local outcome of steering a release (`Cancel` command) — computed at an
/// operation boundary and never stored on the runner across turns. A Cancel
/// preempts the in-flight operation; what happens next is decided right
/// here by whether prompts are queued and by the idle policy. There is no
/// cross-turn "cancelled" state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Steering {
    /// No release happened; continue the turn normally.
    None,
    /// A release happened and prompts are queued: end the current turn so
    /// the outer loop consumes the queued batch immediately; the new turn
    /// decides the session end naturally.
    ReleasedWithPrompts,
    /// A release happened with no prompts queued: WaitForInput returns to
    /// Idle; FinishWhenIdle finalizes `Cancelled` right here (no
    /// "cancelled but waiting forever" intermediate state).
    ReleasedIdle,
}

enum OperationFlow {
    Done(Steering),
    Released(Steering),
    Finished,
}

enum PendingCommand {
    Prompt {
        text: String,
        queued: bool,
        /// Image reference attached by the REPL `/image` entrance; becomes
        /// `Message::User.images` on the committed prompt.
        image: Option<ImagePart>,
    },
    Compact,
    Goal(GoalCommand),
}

pub enum RecoveryScope {
    Session,
    Subagent,
}

pub(crate) struct RecoveryBatch {
    pub(crate) scope: RecoveryScope,
    pub(crate) tasks: Vec<crate::session::UnfinishedTask>,
}

pub(crate) struct SessionBootstrap {
    pub(crate) recovery_batches: Vec<RecoveryBatch>,
    pub(crate) legacy: bool,
    pub(crate) initial_entries: Vec<SessionEntry>,
}

pub struct SessionRunner {
    agent: Agent,
    store: SessionStore,
    root: PathBuf,
    session: String,
    shared: Arc<Mutex<Shared>>,
    commands: mpsc::UnboundedReceiver<SessionCommand>,
    pending: VecDeque<PendingCommand>,
    policy: IdlePolicy,
    last_answer: Option<String>,
    bootstrap: Option<SessionBootstrap>,
    #[cfg(test)]
    before_finalize: Option<Box<dyn FnOnce() + Send>>,
}

impl Drop for SessionRunner {
    fn drop(&mut self) {
        self.shared.lock().unwrap().commands_open = false;
    }
}

impl SessionRunner {
    pub fn new(
        mut agent: Agent,
        store: SessionStore,
        root: PathBuf,
        session: String,
        policy: IdlePolicy,
    ) -> (Self, SessionHandle) {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let (status, _) = watch::channel(SessionStatus::Idle);
        let replay = agent.history().iter().filter_map(entry_event).collect();
        // Mirror the latest goal snapshot so UIs read it without touching
        // the agent (resume fold: newest GoalUpdated wins — reuse the
        // agent's own fold instead of a second reverse scan).
        let goal = agent.goal();
        let shared = Arc::new(Mutex::new(Shared {
            log: replay,
            events,
            status,
            compaction_streaming: false,
            commands_open: true,
            goal,
        }));
        let handler_shared = shared.clone();
        agent.set_event_handler(Box::new(move |event| {
            handler_shared.lock().unwrap().emit_agent(event)
        }));
        let (tx, commands) = mpsc::unbounded_channel();
        let handle = SessionHandle {
            shared: shared.clone(),
            commands: tx,
        };
        (
            Self {
                agent,
                store,
                root,
                session,
                shared,
                commands,
                pending: VecDeque::new(),
                policy,
                last_answer: None,
                bootstrap: None,
                #[cfg(test)]
                before_finalize: None,
            },
            handle,
        )
    }

    pub(crate) async fn new_with_bootstrap(
        agent: Agent,
        store: SessionStore,
        root: PathBuf,
        session: String,
        policy: IdlePolicy,
        bootstrap: SessionBootstrap,
    ) -> anyhow::Result<(Self, SessionHandle)> {
        let (mut runner, handle) = Self::new(agent, store, root, session, policy);
        runner.bootstrap = Some(bootstrap);
        runner.bootstrap().await?;
        Ok((runner, handle))
    }

    async fn bootstrap(&mut self) -> anyhow::Result<()> {
        let Some(bootstrap) = self.bootstrap.take() else {
            return Ok(());
        };
        if bootstrap.legacy {
            self.store
                .rewrite(&self.root, &self.session, self.agent.history())
                .await?;
        }
        for entry in bootstrap.initial_entries {
            self.commit(entry).await?;
        }
        if !bootstrap.recovery_batches.is_empty() {
            let tasks: Vec<&crate::session::UnfinishedTask> = bootstrap
                .recovery_batches
                .iter()
                .flat_map(|batch| batch.tasks.iter())
                .collect();
            let text = format!(
                "[e-agent exited with {} background task(s) still running; they were killed with the process. Re-run them if still needed:]\n{}",
                tasks.len(),
                tasks
                    .iter()
                    .map(|t| crate::session::format_unfinished(
                        t.task_id,
                        &t.label,
                        t.subagent_session_id.as_deref()
                    ))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            self.commit(SessionEntry::Notice { text }).await?;
            for batch in &bootstrap.recovery_batches {
                match batch.scope {
                    RecoveryScope::Session => {
                        self.store
                            .consume_unfinished_background(&self.root, &self.session, &batch.tasks)
                            .await?
                    }
                    RecoveryScope::Subagent => {
                        self.store
                            .consume_unfinished_background_for_subagent(
                                &self.root,
                                &self.session,
                                &batch.tasks,
                            )
                            .await?
                    }
                }
            }
        }
        Ok(())
    }

    pub fn start(mut self, initial_prompt: Option<String>) -> SessionTask {
        if let Some(prompt) = initial_prompt {
            self.pending.push_back(PendingCommand::Prompt {
                text: prompt,
                queued: false,
                image: None,
            });
        }
        SessionTask {
            task: Some(tokio::spawn(async move { self.run().await })),
        }
    }

    async fn commit(&mut self, entry: SessionEntry) -> anyhow::Result<Option<i64>> {
        // Durable append FIRST, then the located key: a receipt emitted by a
        // later provider projection always points at a persisted row
        // (durable-before-ref). The location is `None` only if the backend
        // could not produce one (never on the durable backends).
        let locations = self
            .store
            .append_located(&self.root, &self.session, std::slice::from_ref(&entry))
            .await?;
        let location = locations.into_iter().next();
        // The just-committed entry's real `session_entries.seq` (the ordinal
        // the backend assigned): threaded into `append_usage` so a usage row
        // carries the ACTUAL seq of the assistant/compaction entry it
        // corresponds to. `None` on JSONL, which has no usage table at all —
        // its `append_usage` is a silent no-op, so no usage row is written.
        let committed_seq = location.as_ref().and_then(|loc| match &loc.key {
            LocatedKey::Greptime { seq, .. } | LocatedKey::Sqlite { seq, .. } => Some(*seq),
            LocatedKey::Jsonl { .. } => None,
        });
        let event = match &entry {
            SessionEntry::Message {
                message: Message::User { content, .. },
            } => Some(AgentEvent::UserPrompt(content.clone())),
            SessionEntry::BackgroundCompletion {
                id,
                output,
                label,
                started_at_ms,
                duration_ms,
                exit_code,
                signal,
                status,
                kind,
            } => Some(AgentEvent::BackgroundCompletionNotice {
                id: *id,
                output: output.clone(),
                label: label.clone(),
                started_at_ms: *started_at_ms,
                duration_ms: *duration_ms,
                exit_code: *exit_code,
                signal: signal.clone(),
                status: status.clone(),
                kind: kind.clone(),
            }),
            SessionEntry::Notice { text } => Some(AgentEvent::Notice(text.clone())),
            // Goal updates fan out as one live event after durable commit
            // (UI Notice line + GoalBar refresh), never as a user prompt.
            SessionEntry::GoalUpdated { goal } => {
                self.shared.lock().unwrap().goal = goal.clone();
                Some(AgentEvent::GoalUpdated { goal: goal.clone() })
            }
            _ => None,
        };
        self.agent.apply_entry_located(entry, location);
        if let Some(event) = event {
            // Background notices become live only after their durable entry
            // exists; using Agent's normal event path prevents a second UI-only
            // injection and preserves session fanout semantics.
            self.agent.emit_event(event);
        }
        Ok(committed_seq)
    }

    async fn commit_user_batch(
        &mut self,
        content: String,
        image: Option<ImagePart>,
        consumed: Vec<(bool, String)>,
    ) -> anyhow::Result<()> {
        // Explicit `/image` entrance: a non-vision model cannot consume the
        // image, and committing it would poison every later model call via
        // the vision gate. Reject loudly — the caller surfaces the error to
        // the user and keeps the session alive.
        if image.is_some() && !self.agent.supports_vision() {
            anyhow::bail!("当前模型不支持图片输入");
        }
        let entry: SessionEntry = Message::User {
            content,
            images: image.map_or_else(Vec::new, |image| vec![image]),
        }
        .into();
        let locations = self
            .store
            .append_located(&self.root, &self.session, std::slice::from_ref(&entry))
            .await?;
        let location = locations.into_iter().next();
        self.agent.apply_entry_located(entry, location);
        let mut shared = self.shared.lock().unwrap();
        for (queued, prompt) in consumed {
            if queued {
                shared.emit(AgentEvent::PromptConsumed);
            }
            shared.emit(AgentEvent::UserPrompt(prompt));
        }
        Ok(())
    }

    /// Best-effort durable append of a harness error (provider/model call
    /// failure, compaction failure, image rejection, termination reason).
    /// The entry lands in the session store so a resumed or late-attached
    /// view can audit it, and is fanned out as an `AgentEvent::Error`
    /// through the single shared path — it never enters provider context
    /// (`Agent::context` filters `SessionEntry::Error`). When the append
    /// itself fails, only an eprintln fallback is possible: emit the event
    /// anyway and return. No retry and no recursion: the same root error
    /// is appended at most once.
    async fn commit_error(&mut self, text: String) {
        let entry = SessionEntry::Error { text: text.clone() };
        match self
            .store
            .append_located(&self.root, &self.session, std::slice::from_ref(&entry))
            .await
        {
            Ok(mut locations) => {
                self.agent
                    .apply_entry_located(entry, locations.drain(..).next());
            }
            Err(error) => {
                tracing::warn!("e-agent: cannot persist session error: {error:#}");
                self.agent.apply_entry(entry);
            }
        }
        self.shared.lock().unwrap().emit(AgentEvent::Error(text));
    }

    /// Apply + persist one human goal command. Errors are plain strings
    /// (the caller emits them as `AgentEvent::Error`); the model tool path
    /// reuses the same transition rules under an id + revision CAS.
    async fn apply_goal_command(&mut self, command: GoalCommand) -> Result<(), String> {
        let entry = match command {
            GoalCommand::Create {
                objective,
                success_criteria,
            } => {
                let goal = crate::agent::create_goal(
                    self.agent.goal().as_ref(),
                    objective,
                    success_criteria,
                )?;
                SessionEntry::GoalUpdated { goal: Some(goal) }
            }
            GoalCommand::Action(action) => {
                let current = self.agent.goal();
                let Some(goal) = &current else {
                    return Err("no goal is set for this session".into());
                };
                let next = crate::agent::transition_goal(
                    current.as_ref(),
                    &goal.id,
                    goal.revision,
                    &action,
                    None,
                    Vec::new(),
                )?;
                SessionEntry::GoalUpdated { goal: next }
            }
        };
        self.commit(entry).await.map_err(|e| format!("{e:#}"))?;
        Ok(())
    }

    /// Intercepted `get_goal` / `update_goal` tool execution (the model
    /// never creates goals; updates carry id + revision CAS and the new
    /// snapshot is durably committed here). Both tools are STRICT about
    /// unknown fields: misspelled/extra keys are rejected before any
    /// transition or commit, as a plain model-facing tool error.
    async fn execute_goal_tool(&mut self, call: &ToolCall) -> Result<ToolOutput, String> {
        let arguments: serde_json::Value = serde_json::from_str(&call.arguments)
            .map_err(|error| format!("invalid JSON arguments: {error}"))?;
        // Reject any key outside the tool's allowed set BEFORE anything
        // else: a misspelled field must surface as a plain tool error that
        // names it, never a silent ignore or a half-applied transition.
        fn reject_unknown_goal_fields(
            object: &serde_json::Map<String, serde_json::Value>,
            allowed: &[&str],
            tool: &str,
        ) -> Result<(), String> {
            let unknown: Vec<&str> = object
                .keys()
                .filter(|key| !allowed.contains(&key.as_str()))
                .map(String::as_str)
                .collect();
            if unknown.is_empty() {
                return Ok(());
            }
            Err(format!(
                "{tool} received unknown field(s): {}",
                unknown
                    .iter()
                    .map(|key| format!("`{key}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
        match call.name.as_str() {
            "get_goal" => {
                // get_goal is a read: only the empty object is accepted.
                let object = arguments
                    .as_object()
                    .ok_or("get_goal arguments must be a JSON object")?;
                reject_unknown_goal_fields(object, &[], "get_goal")?;
                Ok(ToolOutput::text(match self.agent.goal() {
                    Some(goal) => crate::agent::goal_snapshot_json(&goal),
                    None => "No goal is set for this session. Goals are created by the user \
                             (/goal set …); you can update an existing one with update_goal."
                        .into(),
                }))
            }
            "update_goal" => {
                let object = arguments
                    .as_object()
                    .ok_or("update_goal arguments must be a JSON object")?;
                reject_unknown_goal_fields(
                    object,
                    &[
                        "id",
                        "revision",
                        "action",
                        "progress",
                        "blocked_reason",
                        "success_criteria",
                        "evidence",
                    ],
                    "update_goal",
                )?;
                let id = object
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("update_goal requires `id` (a string)")?
                    .to_owned();
                let revision = object
                    .get("revision")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or("update_goal requires `revision` (an integer)")?;
                let action = match object.get("action").and_then(serde_json::Value::as_str) {
                    Some("progress") => crate::agent::GoalAction::Progress {
                        progress: object
                            .get("progress")
                            .and_then(serde_json::Value::as_str)
                            .ok_or("action `progress` requires `progress`")?
                            .to_owned(),
                    },
                    Some("pause") => crate::agent::GoalAction::Pause,
                    Some("resume") => crate::agent::GoalAction::Resume,
                    Some("block") => crate::agent::GoalAction::Block {
                        reason: object
                            .get("blocked_reason")
                            .and_then(serde_json::Value::as_str)
                            .ok_or("action `block` requires `blocked_reason`")?
                            .to_owned(),
                    },
                    Some("complete") => crate::agent::GoalAction::Complete,
                    Some("clear") => crate::agent::GoalAction::Clear,
                    other => {
                        return Err(format!(
                            "unknown update_goal action `{}` (known: progress, pause, resume, \
                             block, complete, clear)",
                            other.unwrap_or("")
                        ));
                    }
                };
                // Strict string-array validation: a present but non-array
                // (or non-string-item) `success_criteria` / `evidence` is a
                // plain tool error, never silently filtered.
                fn string_array(
                    object: &serde_json::Map<String, serde_json::Value>,
                    key: &str,
                ) -> Result<Option<Vec<String>>, String> {
                    match object.get(key) {
                        None => Ok(None),
                        Some(serde_json::Value::Array(items)) => {
                            let mut out = Vec::with_capacity(items.len());
                            for item in items {
                                match item.as_str() {
                                    Some(text) => out.push(text.to_owned()),
                                    None => {
                                        return Err(format!("`{key}` must be an array of strings"));
                                    }
                                }
                            }
                            Ok(Some(out))
                        }
                        Some(_) => Err(format!("`{key}` must be an array of strings")),
                    }
                }
                let success_criteria = string_array(object, "success_criteria")?;
                let evidence = string_array(object, "evidence")?.unwrap_or_default();
                let next = crate::agent::transition_goal(
                    self.agent.goal().as_ref(),
                    &id,
                    revision,
                    &action,
                    success_criteria,
                    evidence,
                )?;
                self.commit(SessionEntry::GoalUpdated { goal: next.clone() })
                    .await
                    .map_err(|error| format!("{error:#}"))
                    .map(|_| ())?;
                match next {
                    Some(goal) => Ok(ToolOutput::text(format!(
                        "goal updated:\n{}",
                        crate::agent::goal_projection_text(&goal)
                    ))),
                    None => Ok(ToolOutput::text("goal cleared.")),
                }
            }
            other => Err(format!("unknown goal tool: {other}")),
        }
    }

    /// Intercepted `read_output` tool execution (the always-on read-only
    /// pager for bounded provider projections): resolve the session-local
    /// `eout1` ref (or a historical long ref), read the persisted field,
    /// page it, and render the closed JSON page.
    async fn execute_read_output(&mut self, call: &ToolCall) -> Result<ToolOutput, String> {
        let arguments: serde_json::Value = serde_json::from_str(&call.arguments)
            .map_err(|error| format!("invalid JSON arguments: {error}"))?;
        let (reference, offset, limit) = crate::tools::output::parse_arguments(&arguments)?;
        let text = crate::tools::output::execute(
            &self.store,
            &self.root,
            &self.session,
            &reference,
            offset,
            limit,
        )
        .await?;
        Ok(ToolOutput::text(text))
    }

    fn status(&self, status: SessionStatus) {
        // Turn-boundary metadata touch (R4): the sessions audit table is
        // appended once per return to Idle — not per event (double-write
        // amplification). Fire-and-forget: the write is spawned and never
        // awaited, so losing the final touch at process exit is acceptable
        // (the audit table keeps the last committed snapshot).
        if matches!(status, SessionStatus::Idle) {
            self.store.touch_meta(&self.root, &self.session);
        }
        self.shared.lock().unwrap().status.send_replace(status);
    }
    fn publish_finished(shared: &mut Shared, result: SessionResult) {
        shared.commands_open = false;
        shared.status.send_replace(SessionStatus::Finished(result));
    }

    fn finalize_when_idle(&mut self, result: SessionResult) -> bool {
        #[cfg(test)]
        if let Some(hook) = self.before_finalize.take() {
            hook();
        }
        let command = {
            let mut shared = self.shared.lock().unwrap();
            match self.commands.try_recv() {
                Ok(command) => Some(command),
                Err(mpsc::error::TryRecvError::Empty) => {
                    Self::publish_finished(&mut shared, result);
                    return true;
                }
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    Self::publish_finished(&mut shared, result);
                    return true;
                }
            }
        };
        self.queue(command.expect("received command"));
        false
    }

    async fn terminate(&mut self, result: SessionResult, pending: Vec<SessionCommand>) {
        if let SessionResult::Failed(text) = &result {
            self.commit_error(text.clone()).await;
        }
        self.intake_after_operation(pending);
        loop {
            while self.has_work() {
                match self.pending.front() {
                    Some(PendingCommand::Prompt { .. }) => {
                        let (prompt, image, consumed) = self.take_prompt_batch();
                        if !prompt.is_empty()
                            && let Err(error) =
                                self.commit_user_batch(prompt, image, consumed).await
                        {
                            self.commit_error(format!(
                                "persisting accepted prompt while terminating: {error:#}"
                            ))
                            .await;
                        }
                    }
                    Some(PendingCommand::Compact) => {
                        self.pending.pop_front();
                    }
                    // Goal mutations are dropped while terminating: the
                    // session is ending, nothing will apply them.
                    Some(PendingCommand::Goal(_)) => {
                        self.pending.pop_front();
                    }
                    None => unreachable!(),
                }
            }
            if self.finalize_when_idle(result.clone()) {
                return;
            }
        }
    }

    /// Queue a steering command, returning the local release outcome (see
    /// `Steering`). `Cancel` is a release: it is not queued, and the result
    /// reflects whether prompts are already pending.
    fn queue(&mut self, command: SessionCommand) -> Steering {
        match command {
            SessionCommand::Prompt(prompt) => {
                self.pending.push_back(PendingCommand::Prompt {
                    text: prompt,
                    queued: true,
                    image: None,
                });
                Steering::None
            }
            SessionCommand::PromptWithImage { text, image } => {
                self.pending.push_back(PendingCommand::Prompt {
                    text,
                    queued: true,
                    image: Some(image),
                });
                Steering::None
            }
            SessionCommand::Compact => {
                self.pending.push_back(PendingCommand::Compact);
                Steering::None
            }
            SessionCommand::Goal(command) => {
                self.pending.push_back(PendingCommand::Goal(command));
                Steering::None
            }
            SessionCommand::SwitchModel(model) => {
                // Instant, not queued: the new model applies to the next
                // model call (a call already in flight keeps its model).
                self.agent.set_model(model);
                Steering::None
            }
            SessionCommand::Cancel => self.release_steering(),
        }
    }
    async fn commit_backgrounds(&mut self) -> anyhow::Result<bool> {
        let mut any = false;
        loop {
            // Re-drain on every pass: a completion that arrives while the
            // previous entry's store append is awaiting must not be left in
            // the channel past this flush — it would miss this safety
            // boundary (the next provider call would not see it).
            self.agent.drain_background_ready();
            let Some(entry) = self.agent.peek_background_entry() else {
                return Ok(any);
            };
            // Persist the completion first, but do not apply or publish it
            // until its owner row is durably clear. Resume may therefore
            // never observe a live completion paired with a stale owner.
            let locations = self
                .store
                .append_located(&self.root, &self.session, std::slice::from_ref(&entry))
                .await?;
            let location = locations.into_iter().next();
            self.agent
                .ack_background_entry()
                .await
                .map_err(anyhow::Error::msg)?;
            let event = match &entry {
                SessionEntry::BackgroundCompletion {
                    id,
                    output,
                    label,
                    started_at_ms,
                    duration_ms,
                    exit_code,
                    signal,
                    status,
                    kind,
                } => AgentEvent::BackgroundCompletionNotice {
                    id: *id,
                    output: output.clone(),
                    label: label.clone(),
                    started_at_ms: *started_at_ms,
                    duration_ms: *duration_ms,
                    exit_code: *exit_code,
                    signal: signal.clone(),
                    status: status.clone(),
                    kind: kind.clone(),
                },
                _ => unreachable!("peek_background_entry returns a completion"),
            };
            self.agent.apply_entry_located(entry, location);
            self.agent.emit_event(event);
            any = true;
        }
    }

    fn has_prompt_work(&self) -> bool {
        self.pending
            .iter()
            .any(|command| matches!(command, PendingCommand::Prompt { .. }))
    }

    fn release_steering(&self) -> Steering {
        if self.has_prompt_work() {
            Steering::ReleasedWithPrompts
        } else {
            Steering::ReleasedIdle
        }
    }

    fn drain_ready_commands(&mut self) -> Steering {
        let mut released = false;
        while let Ok(command) = self.commands.try_recv() {
            released |= matches!(command, SessionCommand::Cancel);
            self.queue(command);
        }
        if released {
            self.release_steering()
        } else {
            Steering::None
        }
    }

    fn intake_after_operation(&mut self, pending: Vec<SessionCommand>) -> Steering {
        let mut released = false;
        for command in pending {
            released |= matches!(command, SessionCommand::Cancel);
            self.queue(command);
        }
        released |= self.drain_ready_commands() != Steering::None;
        if released {
            self.release_steering()
        } else {
            Steering::None
        }
    }

    /// Combine two partial steering results (e.g. a single queued command
    /// plus whatever a subsequent drain picked up): any release wins, and
    /// its classification is recomputed from the final queue state so a
    /// prompt arriving after the cancel is still counted as
    /// `ReleasedWithPrompts`.
    fn merge_steering(&self, a: Steering, b: Steering) -> Steering {
        if a == Steering::None && b == Steering::None {
            Steering::None
        } else {
            self.release_steering()
        }
    }

    /// Apply `SwitchModel` commands cached while an operation was in flight,
    /// returning the remaining commands in their original order for the
    /// regular intake. `wait_for_operation` collects commands received
    /// during e.g. a tool execution into `pending`; the switch applies from
    /// the next model call on (history keeps tool images unconditionally, so
    /// no result interpretation depends on the model — non-vision request
    /// copies are stripped at send time).
    fn apply_pending_model_switches(
        &mut self,
        pending: Vec<SessionCommand>,
    ) -> Vec<SessionCommand> {
        let mut deferred = Vec::with_capacity(pending.len());
        for command in pending {
            match command {
                SessionCommand::SwitchModel(model) => self.agent.set_model(model),
                other => deferred.push(other),
            }
        }
        deferred
    }

    fn has_work(&self) -> bool {
        !self.pending.is_empty()
    }

    fn take_prompt_batch(&mut self) -> (String, Option<ImagePart>, Vec<(bool, String)>) {
        let mut prompts = Vec::new();
        let mut consumed = Vec::new();
        let mut image = None;
        while matches!(self.pending.front(), Some(PendingCommand::Prompt { .. })) {
            let Some(PendingCommand::Prompt {
                text,
                queued,
                image: pending_image,
            }) = self.pending.pop_front()
            else {
                unreachable!()
            };
            if image.is_none() {
                image = pending_image;
            }
            if !text.is_empty() {
                consumed.push((queued, text.clone()));
            }
            prompts.push(text);
        }
        (prompts.join("\n\n"), image, consumed)
    }

    /// Handle a release that preempted an in-flight operation (or arrived at
    /// an idle moment), decided from the local `Steering` — there is no
    /// persistent "cancelled" state to carry across turns.
    ///
    /// Returns `true` when the runner must return (FinishWhenIdle finalized
    /// `Cancelled`); `false` means the caller should end the current turn
    /// (`ReleasedWithPrompts`) or just continue (`ReleasedIdle` under
    /// WaitForInput, which returns to Idle and stays alive for future
    /// input).
    fn release_after_preempt(&mut self, steering: Steering) -> bool {
        match steering {
            Steering::None => unreachable!("release_after_preempt requires a release"),
            Steering::ReleasedWithPrompts => {
                // The queued batch is consumed by the outer loop on the next
                // iteration; the turn(s) started from it decide the session
                // end naturally.
                self.shared
                    .lock()
                    .unwrap()
                    .emit(AgentEvent::Notice("processing queued prompts".into()));
                false
            }
            Steering::ReleasedIdle => {
                if self.policy == IdlePolicy::FinishWhenIdle {
                    // Emergency cancel with nothing queued: finalize right
                    // here — no "cancelled but waiting forever" state. The
                    // last `finalize_when_idle` check still gives a
                    // concurrently queued prompt the chance to open a new
                    // turn instead.
                    self.finalize_when_idle(SessionResult::Cancelled)
                } else {
                    self.status(SessionStatus::Idle);
                    false
                }
            }
        }
    }

    async fn compact_operation(&mut self, source: CompactionSource) -> OperationFlow {
        self.status(SessionStatus::Compacting);
        self.shared.lock().unwrap().compaction_streaming = true;
        let waited = await_compaction(&mut self.agent, &mut self.commands).await;
        self.shared.lock().unwrap().compaction_streaming = false;
        match waited.outcome {
            WaitOutcome::Completed(Ok(out)) => {
                let usage = out.usage;
                let projection = entry_event(&out.entry).expect("compaction has a projection");
                let committed_seq = match self.commit(out.entry).await {
                    Ok(seq) => seq,
                    Err(error) => {
                        self.terminate(SessionResult::Failed(format!("{error:#}")), waited.pending)
                            .await;
                        return OperationFlow::Finished;
                    }
                };
                // Publish the complete projection only after durable commit. Streaming
                // deltas were sent live-only while the operation was in flight.
                self.shared.lock().unwrap().emit(projection);
                // 成功压缩后复位 auto-compact 锁存（失败/取消路径走
                // reset_auto_compact_request，此处不动）。refresh_context=false
                // 使 last_context_input 保持压缩前基线（UI 借此标注“压缩前”），
                // 若不复位，下一次普通轮结束时 run loop 用旧基线 ≥80% 判断会
                // 永久抑制自动压缩。复位是安全的防抖：run loop 只在普通轮结束
                // 时重新检查 last_context_input —— 该轮翻篇后新基线 <80% 不再
                // 触发；当前轮仍巨大则下一轮结束后再评估一次，同一轮内不会反复
                // 压缩。
                self.agent.clear_auto_compacted();
                // 压缩用量落盘（kind="compact"）。与 agent.rs 的 `Agent::compact`
                // （直接调用路径，无 store 访问权）不是同一事件：runner 走的是
                // `prepare_compaction`，生产环境压缩只经此处落盘，不会重复写入。
                // seq = the compaction entry's ACTUAL session_entries.seq.
                if let Some(usage) = usage {
                    self.agent.apply_usage(Some(usage.clone()), false);
                    if let Err(error) = self
                        .store
                        .append_usage(
                            &self.root,
                            &self.session,
                            &self.agent.model_name(),
                            "compact",
                            committed_seq,
                            &usage,
                        )
                        .await
                    {
                        tracing::warn!("e-agent: cannot record compaction usage: {error:#}");
                    }
                }
                let steering = self.intake_after_operation(waited.pending);
                self.status(source.resume_status());
                OperationFlow::Done(steering)
            }
            WaitOutcome::Completed(Err(error)) => {
                self.agent.reset_auto_compact_request();
                let text = format!("{}compaction error: {error:#}", source.prefix());
                // Both manual and auto compaction failures are real harness
                // errors: persisted as an Error entry and fanned out as an
                // `AgentEvent::Error` (audit-visible on resume/late attach).
                // A cancel stays a Notice and never lands as an Error entry.
                self.commit_error(text).await;
                self.intake_after_operation(waited.pending);
                self.status(source.resume_status());
                OperationFlow::Done(Steering::None)
            }
            WaitOutcome::Released => {
                // The in-flight compaction future was dropped: no entry, no
                // projection. The release is known (the Cancel was consumed
                // by wait_for_operation); classify from what is now queued.
                self.intake_after_operation(waited.pending);
                let steering = self.release_steering();
                self.agent.reset_auto_compact_request();
                self.shared.lock().unwrap().emit(AgentEvent::Notice(format!(
                    "{}compaction cancelled",
                    source.prefix()
                )));
                self.status(source.resume_status());
                OperationFlow::Released(steering)
            }
            WaitOutcome::Closed => {
                self.terminate(SessionResult::Closed, waited.pending).await;
                OperationFlow::Finished
            }
        }
    }

    async fn run(&mut self) {
        loop {
            match self.commit_backgrounds().await {
                Ok(true) if self.pending.is_empty() => {
                    self.pending.push_back(PendingCommand::Prompt {
                        text: String::new(),
                        queued: false,
                        image: None,
                    })
                }
                Ok(_) => {}
                Err(error) => {
                    self.terminate(SessionResult::Failed(format!("{error:#}")), Vec::new())
                        .await;
                    return;
                }
            }
            let steering = self.drain_ready_commands();
            // A release with nothing queued (no prompts) is handled right
            // here by the policy — even if maintenance (Compact) is pending,
            // an emergency cancel on FinishWhenIdle finalizes Cancelled and
            // the queued Compact is dropped without running (cancel = flush
            // applies to queued user messages, not to internal maintenance
            // commands; pinned by
            // steer_release_with_queued_compact_finish_when_idle_drops_the_compact).
            // ReleasedWithPrompts falls through: the queued batch is consumed
            // below and the turn(s) started from it decide the end naturally.
            if steering == Steering::ReleasedIdle && self.release_after_preempt(steering) {
                return;
            }
            if matches!(self.pending.front(), Some(PendingCommand::Compact)) {
                self.pending.pop_front();
                match self.compact_operation(CompactionSource::Manual).await {
                    OperationFlow::Done(steering) => {
                        if steering != Steering::None && self.release_after_preempt(steering) {
                            return;
                        }
                    }
                    OperationFlow::Released(steering) => {
                        if self.release_after_preempt(steering) {
                            return;
                        }
                        // Queued prompts (if any) are consumed by the outer
                        // loop on the next iteration.
                    }
                    OperationFlow::Finished => return,
                }
                continue;
            }
            // Human goal mutation: apply + persist + fan out, then loop.
            // Must run before take_prompt_batch (an unconsumed Goal at the
            // front would start a turn with an empty prompt).
            if matches!(self.pending.front(), Some(PendingCommand::Goal(_))) {
                let Some(PendingCommand::Goal(command)) = self.pending.pop_front() else {
                    unreachable!()
                };
                if let Err(text) = self.apply_goal_command(command).await {
                    self.shared.lock().unwrap().emit(AgentEvent::Error(text));
                }
                continue;
            }
            if self.pending.is_empty() {
                // An operation may complete in the same scheduling turn as a sender
                // queues follow-up work. Drain every command already ready before
                // applying FinishWhenIdle.
                let steering = self.drain_ready_commands();
                if self.has_work() {
                    continue;
                }
                if steering != Steering::None {
                    if self.release_after_preempt(steering) {
                        return;
                    }
                    continue;
                }
                self.status(SessionStatus::Idle);
                if self.policy == IdlePolicy::FinishWhenIdle
                    && !self.agent.has_blocking_background()
                {
                    let result = SessionResult::Completed(self.last_answer.clone());
                    if self.finalize_when_idle(result) {
                        return;
                    }
                    continue;
                }
                // FinishWhenIdle waits indefinitely for blocking background
                // tasks. Their completion is injected as a follow-up turn.
                tokio::select! { biased;
                    command = self.commands.recv() => match command {
                        Some(command) => {
                            let first = self.queue(command);
                            let drained = self.drain_ready_commands();
                            let steering = self.merge_steering(first, drained);
                            if steering != Steering::None
                                && !self.has_prompt_work()
                                && self.release_after_preempt(steering)
                            {
                                return;
                            }
                            continue;
                        }
                        None => {
                            self.terminate(SessionResult::Closed, Vec::new()).await;
                            return;
                        }
                    },
                    ready = self.agent.wait_background_ready() => {
                        if ready { continue; }
                        self.terminate(SessionResult::Closed, Vec::new()).await;
                        return;
                    }
                }
            }
            let (prompt, image, consumed) = self.take_prompt_batch();
            self.status(SessionStatus::Busy);
            if !prompt.is_empty() {
                let image_rejected = image.is_some() && !self.agent.supports_vision();
                if let Err(error) = self.commit_user_batch(prompt, image, consumed).await {
                    if image_rejected {
                        // Explicit `/image` on a non-vision model is a
                        // user-facing rejection, not a session failure:
                        // surface the error and return to Idle so the user
                        // can retry without the image. Nothing was committed
                        // (a poisoned User message would lock every later
                        // model call behind the vision gate).
                        self.commit_error(format!("{error:#}")).await;
                        continue;
                    }
                    self.terminate(SessionResult::Failed(format!("{error:#}")), Vec::new())
                        .await;
                    return;
                }
            }
            // True turn starts here (fresh/queued prompt, or the idle
            // background-completion follow-up turn from an empty prompt
            // batch): reset per-turn tool state (poll guard).
            // Model rounds, mid-tool-batch, and manual/auto compaction
            // never reset it.
            self.agent.start_turn();
            let specs = self.agent.tool_specs();
            'turn: loop {
                let waited = await_round(&mut self.agent, &specs, &mut self.commands).await;
                let round = match waited.outcome {
                    WaitOutcome::Completed(Ok(round)) => round,
                    WaitOutcome::Completed(Err(error)) => {
                        if self.policy == IdlePolicy::FinishWhenIdle {
                            // 子代理 / 一次性 CLI：无人能继续对话，保持终结失败语义
                            // （delegate.rs 依赖 Finished(Failed) 把失败传回主 agent）。
                            self.terminate(
                                SessionResult::Failed(format!("{error:#}")),
                                waited.pending,
                            )
                            .await;
                            return;
                        }
                        self.intake_after_operation(waited.pending); // 保留排队命令
                        self.commit_error(format!("model call failed: {error:#}"))
                            .await;
                        break 'turn; // 外层循环自然回 Idle
                    }
                    WaitOutcome::Released => {
                        // The in-flight model future was dropped (preempted):
                        // its output is never committed. Queued prompts are
                        // consumed by the outer loop; with none queued the
                        // policy decides right here. (The Cancel itself was
                        // consumed by wait_for_operation, so the release is
                        // known: classify from what is now queued.)
                        self.intake_after_operation(waited.pending);
                        let steering = self.release_steering();
                        self.shared
                            .lock()
                            .unwrap()
                            .emit(AgentEvent::Notice("turn cancelled".into()));
                        if self.release_after_preempt(steering) {
                            return;
                        }
                        break 'turn;
                    }
                    WaitOutcome::Closed => {
                        self.terminate(SessionResult::Closed, waited.pending).await;
                        return;
                    }
                };
                let assistant = round.assistant;
                let usage = round.usage;
                let streamed = round.produced_content_delta;
                let calls = assistant.tool_calls.clone();
                let content = assistant.content.clone();
                if calls.is_empty() {
                    self.last_answer = content.clone();
                }
                let committed_seq = match self.commit(Message::Assistant(assistant).into()).await {
                    Ok(seq) => seq,
                    Err(error) => {
                        self.terminate(SessionResult::Failed(format!("{error:#}")), waited.pending)
                            .await;
                        return;
                    }
                };
                // 正常轮用量落盘（kind="regular"）；持久化失败只告警，不影响会话。
                // seq = the assistant entry's ACTUAL session_entries.seq.
                if let Some(usage) = usage {
                    self.agent.apply_usage(Some(usage.clone()), true);
                    if let Err(error) = self
                        .store
                        .append_usage(
                            &self.root,
                            &self.session,
                            &self.agent.model_name(),
                            "regular",
                            committed_seq,
                            &usage,
                        )
                        .await
                    {
                        tracing::warn!("e-agent: cannot record usage: {error:#}");
                    }
                }
                let steering = self.intake_after_operation(waited.pending);
                if !streamed && let Some(text) = content.filter(|text| !text.is_empty()) {
                    self.agent.emit_event(AgentEvent::AssistantText(text));
                }
                if steering != Steering::None && calls.is_empty() {
                    // Stale release: the round completed naturally (final
                    // answer, no tool calls) and its output is committed —
                    // the committed result wins over the racing cancel
                    // (contract: completed output is never lost). Ignore the
                    // release; the outer loop finalizes normally.
                } else if steering != Steering::None {
                    // The round was committed but the turn still had work
                    // (tool calls / more rounds): the release stops it here.
                    // Queued prompts (if any) are consumed by the outer loop.
                    self.shared
                        .lock()
                        .unwrap()
                        .emit(AgentEvent::Notice("turn cancelled".into()));
                    if self.release_after_preempt(steering) {
                        return;
                    }
                    break 'turn;
                }
                if self.agent.take_auto_compact_request() {
                    self.shared
                        .lock()
                        .unwrap()
                        .emit(AgentEvent::Notice("──── auto-compacting… ────".into()));
                    match self.compact_operation(CompactionSource::Auto).await {
                        OperationFlow::Done(steering) => {
                            if steering != Steering::None {
                                // The compaction completed (its projection was
                                // committed), but the release still stops the
                                // turn here; queued prompts (if any) are
                                // consumed by the outer loop.
                                self.shared
                                    .lock()
                                    .unwrap()
                                    .emit(AgentEvent::Notice("turn cancelled".into()));
                                if self.release_after_preempt(steering) {
                                    return;
                                }
                                break 'turn;
                            }
                        }
                        OperationFlow::Released(steering) => {
                            if self.release_after_preempt(steering) {
                                return;
                            }
                            break 'turn;
                        }
                        OperationFlow::Finished => return,
                    }
                }
                if calls.is_empty() {
                    break 'turn;
                }
                // Poll guard: the terminating unchanged-snapshot
                // get_background_tasks poll (3rd for subagents, 5th for the
                // main agent) returns an internal sentinel.
                // The sentinel never enters history/UI — the committed
                // content is the model-facing POLL_ERROR — and the local
                // latch only fires AFTER the full sibling batch (every call
                // before and after the poll keeps a real ToolResult, so
                // repair_tool_pairs never has to patch a hole) and the
                // commit_backgrounds safe point.
                let mut poll_terminate = false;
                for call in calls {
                    self.agent.emit_event(AgentEvent::ToolCall {
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    });
                    // Goal tools are intercepted by the runner: they need
                    // the session's goal state + durable commit, which a
                    // plain tool cannot reach. They never create goals.
                    if call.name == "get_goal" || call.name == "update_goal" {
                        let result = self.execute_goal_tool(&call).await;
                        let (tool_text, images) = match &result {
                            Ok(output) => (output.content.clone(), output.images.clone()),
                            Err(error) => (error.clone(), Vec::new()),
                        };
                        let is_error = result.is_err();
                        let entry = Message::Tool {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            content: tool_text.clone(),
                            images,
                            is_error,
                            synthetic: false,
                        }
                        .into();
                        if let Err(error) = self.commit(entry).await {
                            self.terminate(SessionResult::Failed(format!("{error:#}")), Vec::new())
                                .await;
                            return;
                        }
                        self.agent.emit_event(AgentEvent::ToolResult {
                            is_error,
                            content: tool_text,
                        });
                        let steering = self.intake_after_operation(Vec::new());
                        if steering != Steering::None {
                            self.shared
                                .lock()
                                .unwrap()
                                .emit(AgentEvent::Notice("turn cancelled".into()));
                            if self.release_after_preempt(steering) {
                                return;
                            }
                            break 'turn;
                        }
                        continue;
                    }
                    // read_output is intercepted by the runner: it needs the
                    // session's store + ref registry (a plain tool cannot
                    // reach them). Its result is committed like any other
                    // tool result — and is itself an eligible persisted
                    // field (`tool_content`), so an oversized page is
                    // bounded with its own receipt in the next request.
                    if call.name == "read_output" {
                        let result = self.execute_read_output(&call).await;
                        let (tool_text, images) = match &result {
                            Ok(output) => (output.content.clone(), output.images.clone()),
                            Err(error) => (error.clone(), Vec::new()),
                        };
                        let is_error = result.is_err();
                        let entry = Message::Tool {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            content: tool_text.clone(),
                            images,
                            is_error,
                            synthetic: false,
                        }
                        .into();
                        if let Err(error) = self.commit(entry).await {
                            self.terminate(SessionResult::Failed(format!("{error:#}")), Vec::new())
                                .await;
                            return;
                        }
                        self.agent.emit_event(AgentEvent::ToolResult {
                            is_error,
                            content: tool_text,
                        });
                        let steering = self.intake_after_operation(Vec::new());
                        if steering != Steering::None {
                            self.shared
                                .lock()
                                .unwrap()
                                .emit(AgentEvent::Notice("turn cancelled".into()));
                            if self.release_after_preempt(steering) {
                                return;
                            }
                            break 'turn;
                        }
                        continue;
                    }
                    let waited = await_tool(&mut self.agent, &call, &mut self.commands).await;
                    let result = match waited.outcome {
                        WaitOutcome::Completed(result) => result,
                        WaitOutcome::Released => {
                            // The in-flight tool future was dropped; the
                            // interrupted tool call is never committed (the
                            // next provider context synthesizes an error
                            // result via repair_tool_pairs). The release is
                            // known (the Cancel was consumed by
                            // wait_for_operation); classify from what is now
                            // queued.
                            self.intake_after_operation(waited.pending);
                            let steering = self.release_steering();
                            self.shared
                                .lock()
                                .unwrap()
                                .emit(AgentEvent::Notice("turn cancelled".into()));
                            if self.release_after_preempt(steering) {
                                return;
                            }
                            break 'turn;
                        }
                        WaitOutcome::Closed => {
                            self.terminate(SessionResult::Closed, waited.pending).await;
                            return;
                        }
                    };
                    // A model switch queued while the tool ran is applied
                    // BEFORE the result is committed; the switch takes
                    // effect from the next model call on (history keeps
                    // tool images unconditionally, so the commit itself does
                    // not depend on the model — the request copy is stripped
                    // for non-vision models at send time).
                    let pending = self.apply_pending_model_switches(waited.pending);
                    if call.name == "get_background_tasks" && is_poll_guard_terminate(&result) {
                        poll_terminate = true;
                    }
                    // One canonical image-bearing Tool entry: the text
                    // summary plus the structured image references ride on
                    // the Tool message itself (no marker parsing, no
                    // synthetic User). Non-vision models never see the
                    // images: the request copy is stripped at send time
                    // (strip_images), while history keeps them so a later
                    // vision model regains them. The poll-guard sentinel is
                    // mapped to the model-facing POLL_ERROR text here so it
                    // never enters the durable entry or the UI.
                    let (tool_text, images) = match &result {
                        Ok(output) => (output.content.clone(), output.images.clone()),
                        Err(error) => (tool_error_content(error).to_owned(), Vec::new()),
                    };
                    let is_error = result.is_err();
                    let entry = Message::Tool {
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        content: tool_text.clone(),
                        images,
                        is_error,
                        synthetic: false,
                    }
                    .into();
                    if let Err(error) = self.commit(entry).await {
                        self.terminate(SessionResult::Failed(format!("{error:#}")), pending)
                            .await;
                        return;
                    }
                    if let Err(error) = self.agent.after_tool_entry(&call, &result).await {
                        self.terminate(SessionResult::Failed(error), pending).await;
                        return;
                    }
                    self.agent.emit_event(AgentEvent::ToolResult {
                        is_error,
                        content: tool_text,
                    });
                    // A release that raced the tool's own completion: the
                    // tool result was committed above (contract: completed
                    // output is never lost), but the release stops the turn
                    // here — the committed result stays in history.
                    let steering = self.intake_after_operation(pending);
                    if steering != Steering::None {
                        self.shared
                            .lock()
                            .unwrap()
                            .emit(AgentEvent::Notice("turn cancelled".into()));
                        if self.release_after_preempt(steering) {
                            return;
                        }
                        break 'turn;
                    }
                }
                // The assistant's full tool-result batch is durably
                // committed: drain + durably commit any background
                // completions that arrived while this batch executed (or
                // during the provider stream that produced it), so the next
                // provider call within this same turn sees them immediately
                // instead of only after the turn ends. This is the safe
                // point — never between the assistant's tool_calls and a
                // real Tool result of the batch. Pending commands (if any)
                // are unaffected: `commit_backgrounds` only drains the
                // agent's background channel.
                if let Err(error) = self.commit_backgrounds().await {
                    self.terminate(SessionResult::Failed(format!("{error:#}")), Vec::new())
                        .await;
                    return;
                }
                // Poll-guard termination: the full sibling batch is durably
                // committed and the safe point ran — only now emit the
                // termination Notice and end the current turn. The next
                // turn (fresh/queued prompt, idle background-completion
                // follow-up) starts with the guard reset and can continue
                // normally.
                if poll_terminate {
                    self.shared
                        .lock()
                        .unwrap()
                        .emit(AgentEvent::Notice(POLL_GUARD_TERMINATION_NOTICE.into()));
                    break 'turn;
                }
            }
        }
    }
}
fn entry_event(entry: &SessionEntry) -> Option<AgentEvent> {
    match entry {
        SessionEntry::Message {
            message: Message::System { .. },
        } => None,
        SessionEntry::Message {
            message: Message::User { content, .. },
        } => Some(AgentEvent::UserPrompt(content.clone())),
        SessionEntry::Message {
            message: Message::Assistant(message),
        } => message.content.clone().map(AgentEvent::AssistantText),
        SessionEntry::Message {
            message: Message::Tool {
                content, is_error, ..
            },
        } => Some(AgentEvent::ToolResult {
            is_error: *is_error,
            content: content.clone(),
        }),
        SessionEntry::Compaction { summary, .. } => {
            Some(AgentEvent::Notice(format!("compacted: {summary}")))
        }
        SessionEntry::Notice { text } => Some(AgentEvent::Notice(text.clone())),
        SessionEntry::BackgroundCompletion {
            id,
            output,
            label,
            started_at_ms,
            duration_ms,
            exit_code,
            signal,
            status,
            kind,
        } => Some(AgentEvent::BackgroundCompletionNotice {
            id: *id,
            output: output.clone(),
            label: label.clone(),
            started_at_ms: *started_at_ms,
            duration_ms: *duration_ms,
            exit_code: *exit_code,
            signal: signal.clone(),
            status: status.clone(),
            kind: kind.clone(),
        }),
        SessionEntry::ForkedFrom { source, at, .. } => Some(AgentEvent::Notice(format!(
            "forked from {source} at entry {at}"
        ))),
        // Harness errors are durable and replay as Error events, so a
        // resumed or late-attached view sees the audit trail.
        SessionEntry::Error { text } => Some(AgentEvent::Error(text.clone())),
        SessionEntry::GoalUpdated { goal } => Some(AgentEvent::GoalUpdated { goal: goal.clone() }),
    }
}

#[cfg(test)]
#[path = "runner_tests.rs"]
mod tests;
