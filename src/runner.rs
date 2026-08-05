//! Durable, single-writer session runner built on the Agent step API.

use crate::{
    agent::{
        Agent, AgentEvent, CompactionOutput, ImagePart, Message, Model, RoundOutput, SessionEntry,
        ToolCall, ToolSpec,
    },
    session_store::SessionStore,
};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{broadcast, mpsc, watch},
    task::JoinHandle,
};

const EVENT_CAPACITY: usize = 256;

enum WaitOutcome<T> {
    Completed(T),
    Cancelled,
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
) -> WaitResult<Result<String, String>> {
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
                    return WaitResult { outcome: WaitOutcome::Cancelled, pending };
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

enum OperationFlow {
    Done,
    Cancelled,
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
}

pub struct SessionRunner {
    agent: Agent,
    store: SessionStore,
    root: PathBuf,
    session: String,
    shared: Arc<Mutex<Shared>>,
    commands: mpsc::UnboundedReceiver<SessionCommand>,
    pending: VecDeque<PendingCommand>,
    cancelled: bool,
    policy: IdlePolicy,
    last_answer: Option<String>,
    /// Upper bound for waiting on blocking background tasks before
    /// finalizing a `FinishWhenIdle` session (`None` = wait indefinitely).
    /// Resolved from `[delegate] finalize_wait_secs` by the session factory;
    /// consumed only at the FinishWhenIdle idle point.
    finalize_wait: Option<Duration>,
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
        let shared = Arc::new(Mutex::new(Shared {
            log: replay,
            events,
            status,
            compaction_streaming: false,
            commands_open: true,
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
                cancelled: false,
                policy,
                last_answer: None,
                finalize_wait: None,
                #[cfg(test)]
                before_finalize: None,
            },
            handle,
        )
    }

    /// Cap the `FinishWhenIdle` wait for blocking background tasks (see
    /// `[delegate] finalize_wait_secs`). `None` keeps the historical
    /// behavior: wait for the background tasks indefinitely.
    pub fn with_finalize_wait(mut self, wait: Option<Duration>) -> Self {
        self.finalize_wait = wait;
        self
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

    async fn commit(&mut self, entry: SessionEntry) -> anyhow::Result<()> {
        self.store
            .append(&self.root, &self.session, std::slice::from_ref(&entry))
            .await?;
        let event = match &entry {
            SessionEntry::Message {
                message: Message::User { content, .. },
            } => Some(AgentEvent::UserPrompt(content.clone())),
            SessionEntry::BackgroundCompletion { id, output, label } => {
                Some(AgentEvent::BackgroundCompletionNotice {
                    id: *id,
                    output: output.clone(),
                    label: label.clone(),
                })
            }
            _ => None,
        };
        self.agent.apply_entry(entry);
        if let Some(event) = event {
            // Background notices become live only after their durable entry
            // exists; using Agent's normal event path prevents a second UI-only
            // injection and preserves session fanout semantics.
            self.agent.emit_event(event);
        }
        Ok(())
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
        self.store
            .append(&self.root, &self.session, std::slice::from_ref(&entry))
            .await?;
        self.agent.apply_entry(entry);
        let mut shared = self.shared.lock().unwrap();
        for (queued, prompt) in consumed {
            if queued {
                shared.emit(AgentEvent::PromptConsumed);
            }
            shared.emit(AgentEvent::UserPrompt(prompt));
        }
        Ok(())
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
            self.shared
                .lock()
                .unwrap()
                .emit(AgentEvent::Error(text.clone()));
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
                            self.shared.lock().unwrap().emit(AgentEvent::Error(format!(
                                "persisting accepted prompt while terminating: {error:#}"
                            )));
                        }
                    }
                    Some(PendingCommand::Compact) => {
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

    fn queue(&mut self, command: SessionCommand) -> bool {
        match command {
            SessionCommand::Prompt(prompt) => {
                self.pending.push_back(PendingCommand::Prompt {
                    text: prompt,
                    queued: true,
                    image: None,
                });
                false
            }
            SessionCommand::PromptWithImage { text, image } => {
                self.pending.push_back(PendingCommand::Prompt {
                    text,
                    queued: true,
                    image: Some(image),
                });
                false
            }
            SessionCommand::Compact => {
                self.pending.push_back(PendingCommand::Compact);
                false
            }
            SessionCommand::SwitchModel(model) => {
                // Instant, not queued: the new model applies to the next
                // model call (a call already in flight keeps its model).
                self.agent.set_model(model);
                false
            }
            SessionCommand::Cancel => {
                self.cancelled = true;
                true
            }
        }
    }
    async fn commit_backgrounds(&mut self) -> anyhow::Result<bool> {
        self.agent.drain_background_ready();
        let mut any = false;
        while let Some(entry) = self.agent.peek_background_entry() {
            self.commit(entry).await?;
            self.agent.ack_background_entry();
            any = true;
        }
        Ok(any)
    }

    fn drain_ready_commands(&mut self) -> bool {
        let mut cancelled = false;
        while let Ok(command) = self.commands.try_recv() {
            cancelled |= self.queue(command);
        }
        cancelled
    }

    fn intake_after_operation(&mut self, pending: Vec<SessionCommand>) -> bool {
        let mut cancelled = false;
        for command in pending {
            cancelled |= self.queue(command);
        }
        self.drain_ready_commands() || cancelled
    }

    /// Apply `SwitchModel` commands cached while an operation was in flight,
    /// returning the remaining commands in their original order for the
    /// regular intake. `wait_for_operation` collects commands received
    /// during e.g. a tool execution into `pending`; a model switch must take
    /// effect BEFORE the tool's result is interpreted, because the vision
    /// guard on read_image results decides with the *new* model. Otherwise a
    /// vision→non-vision switch made mid-execution would still commit the
    /// image (poisoning every later model call through the wire gate), and a
    /// non-vision→vision switch would wrongly drop it.
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

    /// Handle a cancelled turn or an idle moment. `cancelled_by_command`
    /// distinguishes a `SessionCommand::Cancel` interruption (external — the
    /// composer cancel button) from a natural end. Only a natural end under
    /// `FinishWhenIdle` with no running background finalizes; a Cancel-command
    /// interruption returns the session to Idle so a delegate subagent stays
    /// alive for follow-up messages (the "true kill" is the task-panel cancel,
    /// which aborts the runner directly and never goes through this path).
    fn finish_cancelled_or_idle(&mut self, cancelled_by_command: bool) -> bool {
        if !cancelled_by_command
            && self.policy == IdlePolicy::FinishWhenIdle
            && !self.agent.has_blocking_background()
        {
            self.finalize_when_idle(SessionResult::Cancelled)
        } else {
            self.status(SessionStatus::Idle);
            false
        }
    }

    fn stop_turn_for_cancel(&mut self) -> bool {
        if !self.cancelled {
            return false;
        }
        self.shared
            .lock()
            .unwrap()
            .emit(AgentEvent::Notice("turn cancelled".into()));
        if self.has_work() {
            self.status(SessionStatus::Idle);
        } else {
            self.finish_cancelled_or_idle(true);
        }
        true
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
                if let Err(error) = self.commit(out.entry).await {
                    self.terminate(SessionResult::Failed(format!("{error:#}")), waited.pending)
                        .await;
                    return OperationFlow::Finished;
                }
                // Publish the complete projection only after durable commit. Streaming
                // deltas were sent live-only while the operation was in flight.
                self.shared.lock().unwrap().emit(projection);
                self.agent.apply_usage(usage, false);
                self.intake_after_operation(waited.pending);
                self.status(source.resume_status());
                OperationFlow::Done
            }
            WaitOutcome::Completed(Err(error)) => {
                self.agent.reset_auto_compact_request();
                let text = format!("{}compaction error: {error:#}", source.prefix());
                let event = match source {
                    CompactionSource::Manual => AgentEvent::Error(text),
                    CompactionSource::Auto => AgentEvent::Notice(text),
                };
                self.shared.lock().unwrap().emit(event);
                self.intake_after_operation(waited.pending);
                self.status(source.resume_status());
                OperationFlow::Done
            }
            WaitOutcome::Cancelled => {
                self.cancelled = true;
                self.intake_after_operation(waited.pending);
                self.agent.reset_auto_compact_request();
                self.shared.lock().unwrap().emit(AgentEvent::Notice(format!(
                    "{}compaction cancelled",
                    source.prefix()
                )));
                self.status(source.resume_status());
                OperationFlow::Cancelled
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
            let cancelled = self.drain_ready_commands();
            if cancelled && !self.has_work() && self.finish_cancelled_or_idle(true) {
                return;
            }
            if matches!(self.pending.front(), Some(PendingCommand::Compact)) {
                self.pending.pop_front();
                match self.compact_operation(CompactionSource::Manual).await {
                    OperationFlow::Done => {}
                    OperationFlow::Cancelled if self.has_work() => continue,
                    OperationFlow::Cancelled if self.finish_cancelled_or_idle(true) => return,
                    OperationFlow::Cancelled => continue,
                    OperationFlow::Finished => return,
                }
                continue;
            }
            if self.pending.is_empty() {
                // An operation may complete in the same scheduling turn as a sender
                // queues follow-up work. Drain every command already ready before
                // applying FinishWhenIdle.
                let cancelled = self.drain_ready_commands();
                if self.has_work() {
                    continue;
                }
                if cancelled {
                    if self.finish_cancelled_or_idle(true) {
                        return;
                    }
                } else {
                    self.status(SessionStatus::Idle);
                    if self.policy == IdlePolicy::FinishWhenIdle
                        && !self.agent.has_blocking_background()
                        && !self.cancelled
                    {
                        // `self.cancelled` means a Cancel command interrupted
                        // the last turn: the delegate subagent stays alive
                        // (Idle) for follow-up messages instead of finalizing
                        // here. The flag is cleared only when a real prompt
                        // opens a new turn (just below `take_prompt_batch`),
                        // so a later natural completion still finalizes.
                        let result = SessionResult::Completed(self.last_answer.clone());
                        if self.finalize_when_idle(result) {
                            return;
                        }
                        continue;
                    }
                }
                // A FinishWhenIdle session that already finished its work
                // waits here for its blocking background tasks. Cap that
                // wait: a stuck task (hung I/O, `timeout_secs = 0`, zombie
                // grandchild inside bwrap) must not keep a delegate
                // subagent — and its parent task — alive forever. On
                // timeout the session finalizes as Completed WITHOUT
                // cancelling the tasks: they keep running in the shared
                // registry, where the parent agent can still read their
                // output or cancel them from the task panel; completion
                // events delivered to this session's now-closed channel
                // are dropped harmlessly. `None` keeps the historical
                // wait-indefinitely behavior; a Cancel interruption
                // (subagent kept alive for follow-ups) never times out.
                let wait = if self.policy == IdlePolicy::FinishWhenIdle
                    && self.agent.has_blocking_background()
                    && !self.cancelled
                {
                    self.finalize_wait
                } else {
                    None
                };
                let sleep = wait.unwrap_or(Duration::ZERO);
                tokio::select! { biased;
                    command = self.commands.recv() => match command {
                        Some(command) => {
                            let cancelled = self.queue(command);
                            let cancelled = self.drain_ready_commands() || cancelled;
                            if cancelled && !self.has_work() {
                                self.status(SessionStatus::Idle);
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
                    _ = tokio::time::sleep(sleep), if wait.is_some() => {
                        let n = self.agent.background_task_ids().len();
                        self.shared.lock().unwrap().emit(AgentEvent::Notice(format!(
                            "finalizing with {n} background task(s) still running"
                        )));
                        let result = SessionResult::Completed(self.last_answer.clone());
                        if self.finalize_when_idle(result) {
                            return;
                        }
                        continue;
                    }
                }
            }
            let (prompt, image, consumed) = self.take_prompt_batch();
            // A Cancel command interrupted the previous turn; the flag is
            // cleared only now that a real prompt opens a new turn. Pending
            // maintenance work (e.g. Compact) must not clear it, otherwise a
            // FinishWhenIdle session would finalize as soon as the
            // maintenance completes (see the `!self.cancelled` guard at the
            // idle point).
            self.cancelled = false;
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
                        self.shared
                            .lock()
                            .unwrap()
                            .emit(AgentEvent::Error(format!("{error:#}")));
                        continue;
                    }
                    self.terminate(SessionResult::Failed(format!("{error:#}")), Vec::new())
                        .await;
                    return;
                }
            }
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
                        self.shared.lock().unwrap().emit(AgentEvent::Error(
                            // 错误进 shared log + broadcast
                            format!("model call failed: {error:#}"),
                        ));
                        break 'turn; // 外层循环自然回 Idle
                    }
                    WaitOutcome::Cancelled => {
                        self.cancelled = true;
                        self.intake_after_operation(waited.pending);
                        self.shared
                            .lock()
                            .unwrap()
                            .emit(AgentEvent::Notice("turn cancelled".into()));
                        if !self.has_work() && self.finish_cancelled_or_idle(true) {
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
                if let Err(error) = self.commit(Message::Assistant(assistant).into()).await {
                    self.terminate(SessionResult::Failed(format!("{error:#}")), waited.pending)
                        .await;
                    return;
                }
                self.agent.apply_usage(usage, true);
                self.intake_after_operation(waited.pending);
                if !streamed && let Some(text) = content.filter(|text| !text.is_empty()) {
                    self.agent.emit_event(AgentEvent::AssistantText(text));
                }
                if self.stop_turn_for_cancel() {
                    break 'turn;
                }
                if self.agent.take_auto_compact_request() {
                    self.shared
                        .lock()
                        .unwrap()
                        .emit(AgentEvent::Notice("──── auto-compacting… ────".into()));
                    match self.compact_operation(CompactionSource::Auto).await {
                        OperationFlow::Done => {}
                        OperationFlow::Cancelled => {
                            if !self.has_work() && self.finish_cancelled_or_idle(true) {
                                return;
                            }
                            break 'turn;
                        }
                        OperationFlow::Finished => return,
                    }
                    if self.stop_turn_for_cancel() {
                        break 'turn;
                    }
                }
                if calls.is_empty() {
                    break 'turn;
                }
                for call in calls {
                    self.agent.emit_event(AgentEvent::ToolCall {
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    });
                    let waited = await_tool(&mut self.agent, &call, &mut self.commands).await;
                    let result = match waited.outcome {
                        WaitOutcome::Completed(result) => result,
                        WaitOutcome::Cancelled => {
                            self.cancelled = true;
                            self.intake_after_operation(waited.pending);
                            self.shared
                                .lock()
                                .unwrap()
                                .emit(AgentEvent::Notice("turn cancelled".into()));
                            if !self.has_work() && self.finish_cancelled_or_idle(true) {
                                return;
                            }
                            break 'turn;
                        }
                        WaitOutcome::Closed => {
                            self.terminate(SessionResult::Closed, waited.pending).await;
                            return;
                        }
                    };
                    // A model switch queued while the tool ran must apply
                    // BEFORE the result is interpreted: the vision guard
                    // below decides with the *new* model (see
                    // apply_pending_model_switches). Other cached commands
                    // keep their order and are intaken after the commit.
                    let pending = self.apply_pending_model_switches(waited.pending);
                    // A read_image result carries a structured image marker;
                    // strip it so the committed Tool entry and the ToolResult
                    // event keep only the text summary (base64 never reaches
                    // the scrollback), then attach the image as a synthetic
                    // User message right after the tool result (images ride
                    // only on user role). Non-vision models cannot consume
                    // image parts: keep the text summary but skip the
                    // attachment, so the session is not locked out of every
                    // later model call by the vision gate (compaction would
                    // fail the same way).
                    let (mut tool_text, image) = match &result {
                        Ok(content) => crate::agent::split_image_marker(content),
                        Err(error) => (error.clone(), None),
                    };
                    let supports_vision = self.agent.supports_vision();
                    let image = if image.is_some() && !supports_vision {
                        tool_text.push_str("（当前模型不支持图片，已跳过附加）");
                        None
                    } else {
                        image
                    };
                    let entry = Message::Tool {
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        content: tool_text.clone(),
                        is_error: result.is_err(),
                        synthetic: false,
                    }
                    .into();
                    if let Err(error) = self.commit(entry).await {
                        self.terminate(SessionResult::Failed(format!("{error:#}")), pending)
                            .await;
                        return;
                    }
                    self.agent.after_tool_entry(&call, &result);
                    self.agent.emit_event(AgentEvent::ToolResult {
                        is_error: result.is_err(),
                        content: tool_text,
                    });
                    if let Some(image) = image {
                        let path =
                            crate::agent::tool_path_argument(&call.arguments).unwrap_or_default();
                        let user_entry: SessionEntry = Message::User {
                            content: format!("[image attached: {path}]"),
                            images: vec![image],
                        }
                        .into();
                        if let Err(error) = self.commit(user_entry).await {
                            self.terminate(SessionResult::Failed(format!("{error:#}")), pending)
                                .await;
                            return;
                        }
                    }
                    self.intake_after_operation(pending);
                    if self.stop_turn_for_cancel() {
                        break 'turn;
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
        SessionEntry::BackgroundCompletion { id, output, label } => {
            Some(AgentEvent::BackgroundCompletionNotice {
                id: *id,
                output: output.clone(),
                label: label.clone(),
            })
        }
        SessionEntry::ForkedFrom { source, at, .. } => Some(AgentEvent::Notice(format!(
            "forked from {source} at entry {at}"
        ))),
    }
}

#[cfg(test)]
#[path = "runner_tests.rs"]
mod tests;
