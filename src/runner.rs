//! Durable, single-writer session runner built on the Agent step API.

use crate::{
    agent::{
        Agent, AgentEvent, CompactionOutput, GoalStatus, ImagePart, Message, Model,
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

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct UserQuestion {
    pub id: String,
    pub prompt: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct UserInputRequest {
    pub call_id: String,
    pub questions: Vec<UserQuestion>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptSubmission {
    Answered,
    Queued,
    Conflict,
    Closed,
}

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
    /// resolves the profile to a concrete model and its context window;
    /// the runner installs both on the agent together.
    SwitchModel(Box<dyn Model>, Option<u64>),
    /// Human-issued goal mutation (`/goal` commands, web API). The model
    /// never creates goals; its `update_goal` tool is intercepted by the
    /// runner with the same transition rules under an id + revision CAS.
    Goal(GoalCommand),
    /// Structured answer to the currently waiting root input request.
    Answer {
        call_id: String,
        answers: Vec<(String, String)>,
    },
    /// Arm the in-memory continuation driver; `None` means indefinite.
    Continue(Option<u64>),
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
    /// Root-only process-local request; never persisted or recovered.
    WaitingInput(UserInputRequest),
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
    /// Process-local pending root human-input request. It is claimed under
    /// the same mutex as command admission, so answer/cancel cannot race
    /// into two winners.
    waiting_input: Option<UserInputRequest>,
    input_claimed: bool,
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

    /// Submit a web prompt atomically. While waiting for root user input the
    /// prompt is claimed as that answer; otherwise it is queued normally.
    pub fn submit_prompt(&self, prompt: String) -> PromptSubmission {
        self.submit_prompt_with_call_id(None, prompt)
    }

    /// Submit text with an optional call-id binding. A provided id must match
    /// the currently open request; absence is never allowed to infer a wait.
    pub fn submit_prompt_with_call_id(
        &self,
        call_id: Option<String>,
        prompt: String,
    ) -> PromptSubmission {
        let mut shared = self.shared.lock().unwrap();
        if !shared.commands_open || self.commands.is_closed() {
            return PromptSubmission::Closed;
        }
        if let Some(request) = shared.waiting_input.as_ref() {
            if call_id.as_deref() != Some(request.call_id.as_str()) || shared.input_claimed {
                return PromptSubmission::Conflict;
            }
            let id = request.questions[0].id.clone();
            if self
                .commands
                .send(SessionCommand::Answer {
                    call_id: request.call_id.clone(),
                    answers: vec![(id, prompt)],
                })
                .is_err()
            {
                shared.commands_open = false;
                return PromptSubmission::Closed;
            }
            shared.input_claimed = true;
            return PromptSubmission::Answered;
        }
        if call_id.is_some() {
            return PromptSubmission::Conflict;
        }
        let command = SessionCommand::Prompt(prompt.clone());
        if self.commands.send(command).is_err() {
            shared.commands_open = false;
            return PromptSubmission::Closed;
        }
        if matches!(
            *shared.status.borrow(),
            SessionStatus::Busy | SessionStatus::Compacting
        ) {
            shared.emit(AgentEvent::PromptQueued(prompt));
        }
        PromptSubmission::Queued
    }

    /// Queue a prompt with an image attached; the image rides along as a
    /// reference on the resulting `Message::User`.
    pub fn prompt_with_image(&self, prompt: impl Into<String>, image: ImagePart) {
        self.prompt_inner(prompt.into(), Some(image));
    }

    fn prompt_inner(&self, prompt: String, image: Option<ImagePart>) {
        let mut shared = self.shared.lock().unwrap();
        // A waiting request is answerable only through the structured Web
        // entrance; never let another frontend turn it into a normal prompt.
        if shared.waiting_input.is_some() {
            return;
        }
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
        // Claim a waiting request before enqueueing cancel. A concurrent Web
        // answer then deterministically loses (and cannot become a prompt).
        if shared.waiting_input.is_some() {
            shared.input_claimed = true;
        }
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
    /// Switch the session's model and context window at runtime. The caller
    /// resolves the profile (web/TUI `/model`); the runner installs both from
    /// its next call on.
    pub fn switch_model(&self, model: Box<dyn Model>, context_window: Option<u64>) {
        let mut shared = self.shared.lock().unwrap();
        if shared.commands_open
            && !self.commands.is_closed()
            && self
                .commands
                .send(SessionCommand::SwitchModel(model, context_window))
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
    /// Arm the runner-local goal continuation driver. `None` means no cap.
    pub fn continue_goal(&self, budget: Option<u64>) -> bool {
        let mut shared = self.shared.lock().unwrap();
        if shared.commands_open && !self.commands.is_closed() {
            if self
                .commands
                .send(SessionCommand::Continue(budget))
                .is_err()
            {
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
        let mut shared = self.shared.lock().unwrap();
        shared.waiting_input = match &status {
            SessionStatus::WaitingInput(request) => Some(request.clone()),
            _ => None,
        };
        shared.input_claimed = false;
        shared.status.send_replace(status);
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
        waiting_input: None,
        input_claimed: false,
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
    Requested,
}
impl CompactionSource {
    fn prefix(self) -> &'static str {
        match self {
            Self::Manual | Self::Requested => "",
            Self::Auto => "auto-",
        }
    }
    fn resume_status(self) -> SessionStatus {
        match self {
            Self::Manual => SessionStatus::Idle,
            Self::Auto | Self::Requested => SessionStatus::Busy,
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
    Done(Steering, bool),
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
    Continue(Option<u64>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunnerTrigger {
    Goal,
    Background,
    /// Resume an ordinary turn after a queued maintenance operation.
    Resume,
}

pub(crate) struct SessionBootstrap {
    pub(crate) recovery_tasks: Vec<crate::session::UnfinishedTask>,
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
    /// Internal trigger for the next turn; Goal is the in-memory driver mount.
    armed_trigger: Option<RunnerTrigger>,
    /// In-memory driver state. It is never reconstructed or persisted.
    goal_continuation_armed: bool,
    /// None is an indefinite driver; Some is its remaining actual-token cap.
    goal_continuation_remaining: Option<u64>,
    /// A capped continuation spent a provider call without usage. It stops
    /// automatic calls only after already-required sibling results commit.
    goal_continuation_usage_unavailable: bool,
    /// A natural turn end is the only mount point for the driver.
    turn_just_ended: bool,
    /// FIFO maintenance interrupted a live turn; resume it without resetting
    /// per-turn tool state. The Goal trigger still carries its charge class.
    maintenance_resume: bool,
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
            waiting_input: None,
            input_claimed: false,
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
                armed_trigger: None,
                turn_just_ended: false,
                maintenance_resume: false,
                goal_continuation_remaining: None,
                goal_continuation_usage_unavailable: false,
                goal_continuation_armed: false,
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
        if !bootstrap.recovery_tasks.is_empty() {
            let tasks: Vec<&crate::session::UnfinishedTask> =
                bootstrap.recovery_tasks.iter().collect();
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
            self.store
                .consume_unfinished_background(&self.root, &self.session, &bootstrap.recovery_tasks)
                .await?;
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

    /// Arm the runner-local continuation driver. This state is deliberately
    /// ephemeral: it is never represented in history or reconstructed.
    fn arm_goal_continuation(&mut self, budget: Option<u64>) {
        if !matches!(
            self.agent.goal().as_ref().map(|goal| goal.status),
            Some(GoalStatus::Active)
        ) {
            self.goal_continuation_armed = false;
            self.goal_continuation_remaining = None;
            self.goal_continuation_usage_unavailable = false;
            self.armed_trigger = None;
            return;
        }
        self.goal_continuation_armed = true;
        self.goal_continuation_remaining = budget;
        self.goal_continuation_usage_unavailable = false;
        self.shared
            .lock()
            .unwrap()
            .emit(AgentEvent::Notice(match budget {
                Some(budget) => format!("goal continuation armed (token cap: {budget})"),
                None => "goal continuation armed".into(),
            }));
        if !self.agent.has_blocking_background() {
            // Continue owns FIFO maintenance already queued before the next
            // provider call. Prompt intake still clears this trigger, so a
            // real user message remains a User turn.
            self.armed_trigger = Some(RunnerTrigger::Goal);
        }
    }

    /// Cap exhaustion is a runner-local live event: it stays available to
    /// late attaches through Shared's log without becoming a durable entry.
    fn emit_goal_cap_exhausted(&self) {
        self.shared.lock().unwrap().emit(AgentEvent::Notice(
            "goal continuation stopped: token cap exhausted".into(),
        ));
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
        if !matches!(
            self.agent.goal().as_ref().map(|goal| goal.status),
            Some(GoalStatus::Active)
        ) {
            self.goal_continuation_armed = false;
            self.goal_continuation_remaining = None;
            self.goal_continuation_usage_unavailable = false;
            if self.armed_trigger == Some(RunnerTrigger::Resume) && self.maintenance_resume {
                // FIFO maintenance interrupted a human-required tool turn.
                // It resumes that turn, never the now-disarmed driver.
            } else {
                self.armed_trigger = None;
                self.maintenance_resume = false;
            }
            self.turn_just_ended = false;
        }
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

    fn execute_context_usage(&mut self, call: &ToolCall) -> Result<ToolOutput, String> {
        let arguments: serde_json::Value = serde_json::from_str(&call.arguments)
            .map_err(|error| format!("invalid JSON arguments: {error}"))?;
        let object = arguments
            .as_object()
            .ok_or("get_context_usage arguments must be a JSON object")?;
        if !object.is_empty() {
            return Err("get_context_usage accepts no arguments".into());
        }
        let (context_window, input) = self.agent.context_usage();
        let headroom = match (context_window, input) {
            (Some(window), Some(input)) => Some(window.saturating_sub(input)),
            _ => None,
        };
        let utilization = match (context_window, input) {
            (Some(window), Some(input)) if window > 0 => {
                Some(((input as u128 * 100) / window as u128).min(u64::MAX as u128) as u64)
            }
            _ => None,
        };
        Ok(ToolOutput::text(
            serde_json::json!({
                "context_window": context_window,
                "last_reported_input_tokens": input,
                "observed_headroom_tokens": headroom,
                "utilization_percent": utilization,
                "measurement": if input.is_some() { "previous_regular_request" } else { "unavailable" },
                "exact_for_next_request": false,
            })
            .to_string(),
        ))
    }

    async fn execute_request_compaction(
        &mut self,
        call: &ToolCall,
        already_compacted: bool,
        suppressed_for_goal_budget: bool,
    ) -> Result<ToolOutput, String> {
        let arguments: serde_json::Value = serde_json::from_str(&call.arguments)
            .map_err(|error| format!("invalid JSON arguments: {error}"))?;
        match arguments.as_object() {
            Some(object) if object.is_empty() => Ok(ToolOutput::text(if already_compacted {
                "compaction request already satisfied by automatic compaction."
            } else if suppressed_for_goal_budget {
                "compaction request suppressed: goal continuation budget is exhausted or usage is unavailable."
            } else {
                "compaction request accepted for this tool batch."
            })),
            Some(_) => Err("request_compaction accepts no arguments".into()),
            None => Err("request_compaction arguments must be a JSON object".into()),
        }
    }

    /// Intercepted current-session history tool. The binding is entirely
    /// runner-owned: model arguments never select a store, root, or session.
    async fn execute_history_tool(&mut self, call: &ToolCall) -> Result<ToolOutput, String> {
        let arguments: serde_json::Value = serde_json::from_str(&call.arguments)
            .map_err(|error| format!("invalid JSON arguments: {error}"))?;
        let text =
            crate::tools::history::execute(&self.store, &self.root, &self.session, &arguments)
                .await?;
        Ok(ToolOutput::text(text))
    }

    /// Commit and publish a runner-intercepted tool result. The same result
    /// path is used by goal, history, and read_output so their errors retain
    /// the normal persisted tool semantics.
    async fn finish_intercepted_tool(
        &mut self,
        call: &ToolCall,
        result: Result<ToolOutput, String>,
    ) -> anyhow::Result<Steering> {
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
        self.commit(entry).await?;
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
        }
        Ok(steering)
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

    fn begin_waiting_input(&mut self, request: UserInputRequest) {
        let mut shared = self.shared.lock().unwrap();
        shared.input_claimed = false;
        shared.waiting_input = Some(request.clone());
        shared
            .status
            .send_replace(SessionStatus::WaitingInput(request));
    }

    fn clear_waiting_input(&mut self, status: SessionStatus) {
        let mut shared = self.shared.lock().unwrap();
        shared.waiting_input = None;
        shared.input_claimed = false;
        shared.status.send_replace(status);
    }

    async fn await_input_answer(
        &mut self,
        call: &ToolCall,
    ) -> WaitResult<Result<Vec<(String, String)>, String>> {
        let mut pending = Vec::new();
        loop {
            match self.commands.recv().await {
                Some(SessionCommand::Answer { call_id, answers }) if call_id == call.id => {
                    return WaitResult {
                        outcome: WaitOutcome::Completed(Ok(answers)),
                        pending,
                    };
                }
                Some(SessionCommand::Cancel) => {
                    return WaitResult {
                        outcome: WaitOutcome::Released,
                        pending,
                    };
                }
                Some(command) => pending.push(command),
                None => {
                    return WaitResult {
                        outcome: WaitOutcome::Closed,
                        pending,
                    };
                }
            }
        }
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
        self.goal_continuation_armed = false;
        self.goal_continuation_remaining = None;
        self.goal_continuation_usage_unavailable = false;
        self.armed_trigger = None;
        self.maintenance_resume = false;
        self.turn_just_ended = false;
        if let SessionResult::Failed(text) = &result {
            self.commit_error(text.clone()).await;
        }
        self.intake_after_operation(pending);
        loop {
            while self.has_work() {
                match self.pending.front() {
                    Some(PendingCommand::Prompt { .. }) => {
                        let (prompt, image, consumed) = self.take_prompt_batch();
                        if !consumed.is_empty()
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
                    // Goal mutations and continuation resets are dropped
                    // while terminating: the session is ending, nothing will
                    // apply them.
                    Some(PendingCommand::Goal(_)) | Some(PendingCommand::Continue(_)) => {
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
                self.armed_trigger = None;
                self.pending.push_back(PendingCommand::Prompt {
                    text: prompt,
                    queued: true,
                    image: None,
                });
                Steering::None
            }
            SessionCommand::PromptWithImage { text, image } => {
                self.armed_trigger = None;
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
            SessionCommand::Continue(budget) => {
                self.pending.push_back(PendingCommand::Continue(budget));
                Steering::None
            }
            SessionCommand::SwitchModel(model, context_window) => {
                // Instant, not queued: the new model applies to the next
                // model call (a call already in flight keeps its model).
                self.agent.set_model(model, context_window);
                Steering::None
            }
            SessionCommand::Cancel => {
                // Cancel invalidates an already-armed driver and every
                // Continue that was queued before it. A later Continue is
                // queued after this removal and may explicitly re-arm.
                self.pending
                    .retain(|pending| !matches!(pending, PendingCommand::Continue(_)));
                self.invalidate_trigger_for_cancel();
                self.release_steering()
            }
            SessionCommand::Answer { .. } => Steering::None,
        }
    }

    /// Cancel disarms the live continuation without affecting background work.
    fn invalidate_trigger_for_cancel(&mut self) {
        if self.goal_continuation_armed {
            self.shared
                .lock()
                .unwrap()
                .emit(AgentEvent::Notice("goal continuation cancelled".into()));
        }
        self.turn_just_ended = false;
        self.maintenance_resume = false;
        self.armed_trigger = None;
        self.goal_continuation_armed = false;
        self.goal_continuation_remaining = None;
        self.goal_continuation_usage_unavailable = false;
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
                SessionEntry::Notice { text } => AgentEvent::Notice(text.clone()),
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
                _ => unreachable!("peek_background_entry returns a background entry"),
            };
            let completion = matches!(&entry, SessionEntry::BackgroundCompletion { .. });
            self.agent.apply_entry_located(entry, location);
            self.agent.emit_event(event);
            if completion {
                any = true;
            }
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

    /// Consume commands buffered behind a Cancel-released operation. The
    /// Cancel itself was consumed by `wait_for_operation`; its happens-before
    /// relation also invalidates every earlier buffered Continue, while later
    /// commands received through the normal queue may explicitly re-arm.
    fn intake_after_cancel(&mut self, pending: Vec<SessionCommand>) -> Steering {
        self.pending
            .retain(|pending| !matches!(pending, PendingCommand::Continue(_)));
        self.intake_after_operation(
            pending
                .into_iter()
                .filter(|command| !matches!(command, SessionCommand::Continue(_)))
                .collect(),
        )
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
                SessionCommand::SwitchModel(model, context_window) => {
                    self.agent.set_model(model, context_window)
                }
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
            consumed.push((queued, text.clone()));
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

    async fn compact_operation(
        &mut self,
        source: CompactionSource,
        charge_goal_continuation: bool,
    ) -> OperationFlow {
        self.status(SessionStatus::Compacting);
        self.shared.lock().unwrap().compaction_streaming = true;
        let waited = await_compaction(&mut self.agent, &mut self.commands).await;
        self.shared.lock().unwrap().compaction_streaming = false;
        match waited.outcome {
            WaitOutcome::Completed(Ok(out)) => {
                let usage = out.usage;
                if charge_goal_continuation && self.goal_continuation_remaining.is_some() {
                    match usage.as_ref() {
                        Some(usage) => {
                            let used = usage.input_tokens.saturating_add(usage.output_tokens);
                            let exhausted =
                                if let Some(remaining) = &mut self.goal_continuation_remaining {
                                    *remaining = remaining.saturating_sub(used);
                                    *remaining == 0
                                } else {
                                    false
                                };
                            if exhausted {
                                self.emit_goal_cap_exhausted();
                                // This maintenance operation cannot resume the
                                // interrupted turn after consuming its cap.
                                self.maintenance_resume = false;
                            }
                        }
                        None => {
                            self.goal_continuation_armed = false;
                            self.goal_continuation_remaining = None;
                            self.goal_continuation_usage_unavailable = true;
                            self.armed_trigger = None;
                            // The charged maintenance cannot resume its
                            // interrupted turn after usage is unavailable.
                            self.maintenance_resume = false;
                            self.shared.lock().unwrap().emit(AgentEvent::Notice(
                                "goal continuation stopped: model usage unavailable".into(),
                            ));
                        }
                    }
                }
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
                OperationFlow::Done(steering, true)
            }
            WaitOutcome::Completed(Err(error)) => {
                self.agent.reset_auto_compact_request();
                let text = format!("{}compaction error: {error:#}", source.prefix());
                // Both manual and auto compaction failures are real harness
                // errors: persisted as an Error entry and fanned out as an
                // `AgentEvent::Error` (audit-visible on resume/late attach).
                // A cancel stays a Notice and never lands as an Error entry.
                self.commit_error(text).await;
                let steering = self.intake_after_operation(waited.pending);
                self.status(source.resume_status());
                OperationFlow::Done(steering, false)
            }
            WaitOutcome::Released => {
                self.invalidate_trigger_for_cancel();
                // The in-flight compaction future was dropped: no entry, no
                // projection. The release is known (the Cancel was consumed
                // by wait_for_operation); classify from what is now queued.
                self.intake_after_cancel(waited.pending);
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
            // An armed Goal has a single precedence boundary: commands that
            // are already ready get first refusal. Compact/Goal commands are
            // handled below in FIFO order; only after that queue is clear do
            // newly-ready backgrounds replace the Goal trigger.
            let goal_boundary = self.armed_trigger == Some(RunnerTrigger::Goal);
            if !goal_boundary {
                match self.commit_backgrounds().await {
                    Ok(true)
                        if !(self.has_prompt_work()
                            || self.armed_trigger == Some(RunnerTrigger::Resume)
                                && self.maintenance_resume) =>
                    {
                        // A completion gets exactly one ordinary follow-up even
                        // when maintenance is queued. A later real prompt
                        // clears this marker and consumes the completion in
                        // the User turn instead.
                        self.armed_trigger = Some(RunnerTrigger::Background);
                    }
                    Ok(_) => {}
                    Err(error) => {
                        self.terminate(SessionResult::Failed(format!("{error:#}")), Vec::new())
                            .await;
                        return;
                    }
                }
            }
            let mut steering = self.drain_ready_commands();
            if goal_boundary && self.armed_trigger != Some(RunnerTrigger::Goal) {
                // The first drain may invalidate an armed Goal with a real
                // prompt or Cancel. Do not release/finalize or start that
                // prompt until every completion already ready at this
                // boundary is durably committed. A real prompt consumes it in
                // the User call; it must not get a separate Background turn.
                match self.commit_backgrounds().await {
                    Ok(committed) => {
                        if committed && !self.has_prompt_work() {
                            self.armed_trigger = Some(RunnerTrigger::Background);
                        }
                    }
                    Err(error) => {
                        self.terminate(SessionResult::Failed(format!("{error:#}")), Vec::new())
                            .await;
                        return;
                    }
                }
                // Store I/O above can give a racing command a chance to
                // arrive. Reclassify after the durable boundary so Cancel
                // cannot finalize early and a prompt cannot miss this User
                // call's completion injection.
                let drained = self.drain_ready_commands();
                steering = self.merge_steering(steering, drained);
            }
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
                // A manual compaction queued while the driver is armed is
                // charged to its cap. At an exhausted boundary the driver is
                // resolved first, so the explicitly requested maintenance is
                // ordinary work rather than an uncharged continuation call.
                let charge_goal_continuation = self.armed_trigger == Some(RunnerTrigger::Goal)
                    && self.goal_continuation_armed
                    && self.goal_continuation_remaining != Some(0);
                if self.goal_continuation_remaining == Some(0) {
                    self.goal_continuation_armed = false;
                    self.goal_continuation_remaining = None;
                    // A human-required continuation may be resuming through
                    // this FIFO compaction; do not discard its genuine
                    // same-turn resume while resolving the exhausted driver.
                    if self.armed_trigger != Some(RunnerTrigger::Resume) {
                        self.maintenance_resume = false;
                        self.armed_trigger = None;
                    }
                }
                match self
                    .compact_operation(CompactionSource::Manual, charge_goal_continuation)
                    .await
                {
                    OperationFlow::Done(steering, _) => {
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
            if matches!(self.pending.front(), Some(PendingCommand::Continue(_))) {
                let Some(PendingCommand::Continue(budget)) = self.pending.pop_front() else {
                    unreachable!()
                };
                self.arm_goal_continuation(budget);
                continue;
            }
            if self.pending.is_empty() && goal_boundary {
                // Finish the Goal precedence boundary exactly once. A
                // committed completion coalesces all ready entries into one
                // Background follow-up and supersedes the Goal charge.
                match self.commit_backgrounds().await {
                    Ok(committed) => {
                        if committed {
                            self.armed_trigger = Some(RunnerTrigger::Background);
                        }
                    }
                    Err(error) => {
                        self.terminate(SessionResult::Failed(format!("{error:#}")), Vec::new())
                            .await;
                        return;
                    }
                };
                let steering = self.drain_ready_commands();
                if self.has_work() || steering != Steering::None {
                    if steering != Steering::None && self.release_after_preempt(steering) {
                        return;
                    }
                    continue;
                }
            }
            if self.pending.is_empty() && self.armed_trigger == Some(RunnerTrigger::Goal) {
                // Goal mutations at the precedence boundary can make the
                // already-armed continuation ineligible. Drop it before the
                // provider call, without charging or emitting its Notice.
                let eligible = self.goal_continuation_armed
                    && matches!(
                        self.agent.goal().as_ref().map(|goal| goal.status),
                        Some(GoalStatus::Active)
                    )
                    && !self.agent.has_blocking_background()
                    && self.goal_continuation_remaining != Some(0)
                    && !self.goal_continuation_usage_unavailable;
                if !eligible {
                    self.armed_trigger = None;
                    self.turn_just_ended = false;
                    if self.goal_continuation_remaining == Some(0) {
                        self.goal_continuation_armed = false;
                        self.goal_continuation_remaining = None;
                        self.goal_continuation_usage_unavailable = false;
                    }
                    continue;
                }
                // Continuation start boundary: commands observed here win
                // without issuing the Goal provider call. After this drain,
                // the provider-call transition is the linearization point for
                // a continuation racing another sender.
                let steering = self.drain_ready_commands();
                if self.has_work() || steering != Steering::None {
                    if steering == Steering::ReleasedIdle && self.release_after_preempt(steering) {
                        return;
                    }
                    continue;
                }
            }
            if self.pending.is_empty() && self.armed_trigger.is_none() {
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
                // The driver mounts only after a natural turn end. An active
                // goal by itself never arms it.
                if self.turn_just_ended && self.goal_continuation_armed {
                    let goal_active = matches!(
                        self.agent.goal().as_ref().map(|goal| goal.status),
                        Some(GoalStatus::Active)
                    );
                    if goal_active
                        && self.goal_continuation_remaining != Some(0)
                        && !self.goal_continuation_usage_unavailable
                    {
                        if !self.agent.has_blocking_background() {
                            self.armed_trigger = Some(RunnerTrigger::Goal);
                            continue;
                        }
                        // Owned background work suspends the live driver. Its
                        // completion gets the ordinary follow-up, whose
                        // natural end mounts this still-armed Goal again.
                    } else {
                        self.goal_continuation_armed = false;
                        self.goal_continuation_remaining = None;
                        self.armed_trigger = None;
                    }
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
            // A trigger classifies only an empty pending batch. A real user
            // batch always starts a User turn, even when maintenance retained
            // the Goal trigger behind it.
            let trigger = self.armed_trigger.take();
            let goal_turn = consumed.is_empty() && trigger == Some(RunnerTrigger::Goal);
            let resume_turn = consumed.is_empty()
                && (trigger == Some(RunnerTrigger::Resume) || self.maintenance_resume);
            self.maintenance_resume = false;
            self.turn_just_ended = false;

            self.status(SessionStatus::Busy);
            if !consumed.is_empty() {
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
            if !resume_turn {
                self.agent.start_turn();
            }
            let specs = self.agent.tool_specs();
            // A valid answer is human-owned work required to complete the
            // already-committed tool call. It may make one (or more tool-
            // coupled) model rounds even when the Goal driver's cap stopped.
            // FIFO maintenance resumes that same work through Resume.
            let mut human_required_continuation =
                resume_turn && trigger == Some(RunnerTrigger::Resume);
            'turn: loop {
                // A capped continuation may overshoot on the call that spends
                // its final tokens. This guard applies only to its own Goal
                // turn; a queued human prompt always starts a regular turn.
                if goal_turn
                    && !human_required_continuation
                    && self.goal_continuation_armed
                    && self.goal_continuation_remaining == Some(0)
                {
                    self.goal_continuation_armed = false;
                    self.goal_continuation_remaining = None;
                    self.goal_continuation_usage_unavailable = false;
                    self.armed_trigger = None;
                    break 'turn;
                }
                if goal_turn
                    && !human_required_continuation
                    && self.goal_continuation_usage_unavailable
                {
                    break 'turn;
                }
                let waited = await_round(&mut self.agent, &specs, &mut self.commands).await;
                let round = match waited.outcome {
                    WaitOutcome::Completed(Ok(round)) => round,
                    WaitOutcome::Completed(Err(error)) => {
                        self.goal_continuation_armed = false;
                        self.goal_continuation_remaining = None;
                        self.armed_trigger = None;
                        self.turn_just_ended = false;
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
                        self.invalidate_trigger_for_cancel();
                        // The in-flight model future was dropped (preempted):
                        // its output is never committed. Queued prompts are
                        // consumed by the outer loop; with none queued the
                        // policy decides right here. (The Cancel itself was
                        // consumed by wait_for_operation, so the release is
                        // known: classify from what is now queued.)
                        self.intake_after_cancel(waited.pending);
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
                if goal_turn && !human_required_continuation && self.goal_continuation_armed {
                    match usage.as_ref() {
                        Some(usage) => {
                            let used = usage.input_tokens.saturating_add(usage.output_tokens);
                            let exhausted =
                                if let Some(remaining) = &mut self.goal_continuation_remaining {
                                    *remaining = remaining.saturating_sub(used);
                                    *remaining == 0
                                } else {
                                    false
                                };
                            if exhausted {
                                self.emit_goal_cap_exhausted();
                            }
                        }
                        None if self.goal_continuation_remaining.is_some() => {
                            self.goal_continuation_usage_unavailable = true;
                            self.goal_continuation_armed = false;
                            self.goal_continuation_remaining = None;
                            self.armed_trigger = None;
                            self.shared.lock().unwrap().emit(AgentEvent::Notice(
                                "goal continuation stopped: model usage unavailable".into(),
                            ));
                        }
                        None => {}
                    }
                }
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
                self.agent.apply_usage(usage.clone(), true);
                if let Some(usage) = usage
                    && let Err(error) = self
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
                let steering = self.intake_after_operation(waited.pending);
                if !streamed && let Some(text) = content.clone().filter(|text| !text.is_empty()) {
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
                let mut auto_compacted = false;
                let auto_compaction_allowed = !goal_turn
                    || human_required_continuation
                    || (self.goal_continuation_remaining != Some(0)
                        && !self.goal_continuation_usage_unavailable);
                if auto_compaction_allowed && self.agent.take_auto_compact_request() {
                    self.shared
                        .lock()
                        .unwrap()
                        .emit(AgentEvent::Notice("──── auto-compacting… ────".into()));
                    match self
                        .compact_operation(
                            CompactionSource::Auto,
                            goal_turn && !human_required_continuation,
                        )
                        .await
                    {
                        OperationFlow::Done(steering, committed) => {
                            auto_compacted = committed;
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
                    // Natural turn end is the only point at which the
                    // armed driver may mount its next Goal turn. This applies
                    // equally after a background follow-up.
                    if steering == Steering::None {
                        self.turn_just_ended = self.goal_continuation_armed;
                    }
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
                let mut requested_compaction = false;
                for call in calls {
                    self.agent.emit_event(AgentEvent::ToolCall {
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    });
                    // Root-only marker: persist the assistant ToolCall first,
                    // then pause this same model/tool turn for the Web answer.
                    if call.name == "request_user_input" {
                        let questions = match crate::tools::parse_questions(&call.arguments) {
                            Ok(questions) => questions,
                            Err(error) => {
                                let entry = Message::Tool {
                                    call_id: call.id.clone(),
                                    name: call.name.clone(),
                                    content: error.clone(),
                                    images: Vec::new(),
                                    is_error: true,
                                    synthetic: false,
                                }
                                .into();
                                if let Err(error) = self.commit(entry).await {
                                    self.terminate(
                                        SessionResult::Failed(format!("{error:#}")),
                                        Vec::new(),
                                    )
                                    .await;
                                    return;
                                }
                                self.agent.emit_event(AgentEvent::ToolResult {
                                    is_error: true,
                                    content: error,
                                });
                                continue;
                            }
                        };
                        self.begin_waiting_input(UserInputRequest {
                            call_id: call.id.clone(),
                            questions: questions.clone(),
                        });
                        let waited = self.await_input_answer(&call).await;
                        match waited.outcome {
                            WaitOutcome::Completed(Ok(answers)) => {
                                let answer = answers.into_iter().next();
                                let Some((id, value)) = answer else {
                                    self.clear_waiting_input(SessionStatus::Busy);
                                    continue;
                                };
                                let expected = questions[0].id.clone();
                                if id != expected || value.trim().is_empty() {
                                    self.clear_waiting_input(SessionStatus::Busy);
                                    continue;
                                }
                                let content =
                                    serde_json::json!({"answers": [{"id": id, "value": value}]})
                                        .to_string();
                                let entry = Message::Tool {
                                    call_id: call.id.clone(),
                                    name: call.name.clone(),
                                    content: content.clone(),
                                    images: Vec::new(),
                                    is_error: false,
                                    synthetic: false,
                                }
                                .into();
                                if let Err(error) = self.commit(entry).await {
                                    self.terminate(
                                        SessionResult::Failed(format!("{error:#}")),
                                        waited.pending,
                                    )
                                    .await;
                                    return;
                                }
                                self.clear_waiting_input(SessionStatus::Busy);
                                human_required_continuation = true;
                                self.agent.emit_event(AgentEvent::ToolResult {
                                    is_error: false,
                                    content,
                                });
                                let steering = self.intake_after_operation(waited.pending);
                                if steering != Steering::None {
                                    break 'turn;
                                }
                                continue;
                            }
                            WaitOutcome::Released => {
                                self.clear_waiting_input(SessionStatus::Idle);
                                self.invalidate_trigger_for_cancel();
                                self.intake_after_cancel(waited.pending);
                                self.shared
                                    .lock()
                                    .unwrap()
                                    .emit(AgentEvent::Notice("turn cancelled".into()));
                                break 'turn;
                            }
                            WaitOutcome::Closed => {
                                self.clear_waiting_input(SessionStatus::Idle);
                                self.terminate(SessionResult::Closed, waited.pending).await;
                                return;
                            }
                            WaitOutcome::Completed(Err(error)) => {
                                self.clear_waiting_input(SessionStatus::Busy);
                                self.shared.lock().unwrap().emit(AgentEvent::Error(error));
                                break 'turn;
                            }
                        }
                    }
                    if call.name == "get_context_usage" {
                        let result = self.execute_context_usage(&call);
                        match self.finish_intercepted_tool(&call, result).await {
                            Ok(Steering::None) => {}
                            Ok(steering) => {
                                if self.release_after_preempt(steering) {
                                    return;
                                }
                                break 'turn;
                            }
                            Err(error) => {
                                self.terminate(
                                    SessionResult::Failed(format!("{error:#}")),
                                    Vec::new(),
                                )
                                .await;
                                return;
                            }
                        }
                        continue;
                    }
                    if call.name == "request_compaction" {
                        let request_compaction_allowed = !goal_turn
                            || human_required_continuation
                            || (self.goal_continuation_remaining != Some(0)
                                && !self.goal_continuation_usage_unavailable);
                        let result = self
                            .execute_request_compaction(
                                &call,
                                auto_compacted,
                                !request_compaction_allowed,
                            )
                            .await;
                        if result.is_ok() && request_compaction_allowed {
                            requested_compaction = true;
                        }
                        match self.finish_intercepted_tool(&call, result).await {
                            Ok(Steering::None) => {}
                            Ok(steering) => {
                                if self.release_after_preempt(steering) {
                                    return;
                                }
                                break 'turn;
                            }
                            Err(error) => {
                                self.terminate(
                                    SessionResult::Failed(format!("{error:#}")),
                                    Vec::new(),
                                )
                                .await;
                                return;
                            }
                        }
                        continue;
                    }
                    // Goal tools are intercepted by the runner: they need
                    // the session's goal state + durable commit, which a
                    // plain tool cannot reach. They never create goals.
                    if call.name == "get_goal" || call.name == "update_goal" {
                        let result = self.execute_goal_tool(&call).await;
                        match self.finish_intercepted_tool(&call, result).await {
                            Ok(Steering::None) => {}
                            Ok(steering) => {
                                if self.release_after_preempt(steering) {
                                    return;
                                }
                                break 'turn;
                            }
                            Err(error) => {
                                self.terminate(
                                    SessionResult::Failed(format!("{error:#}")),
                                    Vec::new(),
                                )
                                .await;
                                return;
                            }
                        }
                        continue;
                    }
                    // history is intercepted by the runner: it is bound to
                    // this session's store/root/session and cannot be pointed
                    // elsewhere by model arguments.
                    if call.name == "history" {
                        let result = self.execute_history_tool(&call).await;
                        match self.finish_intercepted_tool(&call, result).await {
                            Ok(Steering::None) => {}
                            Ok(steering) => {
                                if self.release_after_preempt(steering) {
                                    return;
                                }
                                break 'turn;
                            }
                            Err(error) => {
                                self.terminate(
                                    SessionResult::Failed(format!("{error:#}")),
                                    Vec::new(),
                                )
                                .await;
                                return;
                            }
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
                        match self.finish_intercepted_tool(&call, result).await {
                            Ok(Steering::None) => {}
                            Ok(steering) => {
                                if self.release_after_preempt(steering) {
                                    return;
                                }
                                break 'turn;
                            }
                            Err(error) => {
                                self.terminate(
                                    SessionResult::Failed(format!("{error:#}")),
                                    Vec::new(),
                                )
                                .await;
                                return;
                            }
                        }
                        continue;
                    }
                    let waited = await_tool(&mut self.agent, &call, &mut self.commands).await;
                    let result = match waited.outcome {
                        WaitOutcome::Completed(result) => result,
                        WaitOutcome::Released => {
                            self.invalidate_trigger_for_cancel();
                            // The in-flight tool future was dropped; the
                            // interrupted tool call is never committed (the
                            // next provider context synthesizes an error
                            // result via repair_tool_pairs). The release is
                            // known (the Cancel was consumed by
                            // wait_for_operation); classify from what is now
                            // queued.
                            self.intake_after_cancel(waited.pending);
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
                match self.commit_backgrounds().await {
                    Ok(true) => {}
                    Ok(false) => {}
                    Err(error) => {
                        self.terminate(SessionResult::Failed(format!("{error:#}")), Vec::new())
                            .await;
                        return;
                    }
                }
                // This full sibling-batch safe point is also the last
                // chance for steering before another automatic Goal call.
                // Defer queued Prompt/Goal/Compact work to the outer FIFO
                // loop only after every real sibling result was committed.
                let steering = self.drain_ready_commands();
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
                // An inactive goal must disarm before FIFO maintenance
                // can hand the turn off. A valid answer may still resume its
                // own tool turn; ordinary Goal work stops after this batch.
                let goal_inactive = !matches!(
                    self.agent.goal().as_ref().map(|goal| goal.status),
                    Some(GoalStatus::Active)
                );
                if goal_inactive && self.goal_continuation_armed {
                    self.goal_continuation_armed = false;
                    self.goal_continuation_remaining = None;
                    self.goal_continuation_usage_unavailable = false;
                    self.turn_just_ended = false;
                    if !human_required_continuation {
                        self.armed_trigger = None;
                        self.maintenance_resume = false;
                    }
                }
                if self.has_work() {
                    if requested_compaction
                        && !auto_compacted
                        && !self
                            .pending
                            .iter()
                            .any(|command| matches!(command, PendingCommand::Compact))
                        && self.pending.iter().any(|command| {
                            matches!(
                                command,
                                PendingCommand::Prompt { .. } | PendingCommand::Goal(_)
                            )
                        })
                    {
                        self.shared.lock().unwrap().emit(AgentEvent::Notice(
                            "compaction request superseded by queued human work".into(),
                        ));
                    }
                    // A queued compact is maintenance inside this existing
                    // turn, not its conclusion. Reopen a blank ordinary or
                    // Goal turn after the FIFO compaction completes.
                    if human_required_continuation {
                        // Accepted input remains owned by this tool turn even
                        // when FIFO maintenance follows an exhausted Goal
                        // driver. Resume without rearming or resetting tools.
                        self.armed_trigger = Some(RunnerTrigger::Resume);
                        self.maintenance_resume = true;
                    } else if goal_turn
                        && self.goal_continuation_armed
                        && self.goal_continuation_remaining != Some(0)
                    {
                        // Goal commands and compaction are FIFO maintenance
                        // within this interrupted Goal turn. A successful
                        // pause/clear clears the trigger in apply_goal_command.
                        self.armed_trigger = Some(RunnerTrigger::Goal);
                        self.maintenance_resume = true;
                    } else if !goal_turn
                        && matches!(self.pending.front(), Some(PendingCommand::Compact))
                    {
                        self.armed_trigger = Some(RunnerTrigger::Resume);
                    }
                    break 'turn;
                }
                // A goal update in this sibling batch can make the driver
                // ineligible. Stop only after every sibling result and any
                // background completion have been committed.
                if goal_turn && goal_inactive && !human_required_continuation {
                    break 'turn;
                }
                // Poll-guard termination: the full sibling batch is durably
                // committed and the safe point ran — only now emit the
                // termination Notice and end the current turn. The next
                // turn (fresh/queued prompt, idle background-completion
                // follow-up) starts with the guard reset and can continue
                // normally.
                if poll_terminate {
                    self.turn_just_ended = self.goal_continuation_armed;
                    self.shared
                        .lock()
                        .unwrap()
                        .emit(AgentEvent::Notice(POLL_GUARD_TERMINATION_NOTICE.into()));
                    break 'turn;
                }
                if requested_compaction && !auto_compacted {
                    match self
                        .compact_operation(
                            CompactionSource::Requested,
                            goal_turn && !human_required_continuation,
                        )
                        .await
                    {
                        OperationFlow::Done(steering, _) => {
                            if steering != Steering::None {
                                if self.release_after_preempt(steering) {
                                    return;
                                }
                                break 'turn;
                            }
                            // Completion may arrive while compaction was in
                            // flight; commit it before the next provider call.
                            if let Err(error) = self.commit_backgrounds().await {
                                self.terminate(
                                    SessionResult::Failed(format!("{error:#}")),
                                    Vec::new(),
                                )
                                .await;
                                return;
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
