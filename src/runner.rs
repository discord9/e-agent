//! Durable, single-writer session runner built on the Agent step API.

use crate::{
    agent::{
        Agent, AgentEvent, CompactionOutput, Message, RoundOutput, SessionEntry, ToolCall, ToolSpec,
    },
    session_store::SessionStore,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionCommand {
    Prompt(String),
    Cancel,
    Compact,
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
        let prompt = prompt.into();
        let mut shared = self.shared.lock().unwrap();
        if !shared.commands_open || self.commands.is_closed() {
            return;
        }
        if self
            .commands
            .send(SessionCommand::Prompt(prompt.clone()))
            .is_ok()
        {
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
impl Drop for SessionTask {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
impl SessionTask {
    pub async fn join(mut self) -> Result<(), tokio::task::JoinError> {
        let result = self.task.as_mut().expect("task already joined").await;
        self.task = None;
        result
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
    Prompt { text: String, queued: bool },
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
                #[cfg(test)]
                before_finalize: None,
            },
            handle,
        )
    }

    pub fn start(mut self, initial_prompt: Option<String>) -> SessionTask {
        if let Some(prompt) = initial_prompt {
            self.pending.push_back(PendingCommand::Prompt {
                text: prompt,
                queued: false,
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
                message: Message::User { content },
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
        consumed: Vec<(bool, String)>,
    ) -> anyhow::Result<()> {
        let entry: SessionEntry = Message::User { content }.into();
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
                        let (prompt, consumed) = self.take_prompt_batch();
                        if !prompt.is_empty()
                            && let Err(error) = self.commit_user_batch(prompt, consumed).await
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
                });
                false
            }
            SessionCommand::Compact => {
                self.pending.push_back(PendingCommand::Compact);
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

    fn has_work(&self) -> bool {
        !self.pending.is_empty()
    }

    fn take_prompt_batch(&mut self) -> (String, Vec<(bool, String)>) {
        let mut prompts = Vec::new();
        let mut consumed = Vec::new();
        while matches!(self.pending.front(), Some(PendingCommand::Prompt { .. })) {
            let Some(PendingCommand::Prompt { text, queued }) = self.pending.pop_front() else {
                unreachable!()
            };
            if !text.is_empty() {
                consumed.push((queued, text.clone()));
            }
            prompts.push(text);
        }
        (prompts.join("\n\n"), consumed)
    }

    fn finish_cancelled_or_idle(&mut self) -> bool {
        if self.policy == IdlePolicy::FinishWhenIdle && !self.agent.has_running_background() {
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
            self.finish_cancelled_or_idle();
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
            if self.has_work() {
                self.cancelled = false;
            }
            if cancelled && !self.has_work() && self.finish_cancelled_or_idle() {
                return;
            }
            if matches!(self.pending.front(), Some(PendingCommand::Compact)) {
                self.pending.pop_front();
                match self.compact_operation(CompactionSource::Manual).await {
                    OperationFlow::Done => {}
                    OperationFlow::Cancelled if self.has_work() => continue,
                    OperationFlow::Cancelled if self.finish_cancelled_or_idle() => return,
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
                    if self.finish_cancelled_or_idle() {
                        return;
                    }
                } else {
                    self.status(SessionStatus::Idle);
                    if self.policy == IdlePolicy::FinishWhenIdle
                        && !self.agent.has_running_background()
                    {
                        let result = if self.cancelled {
                            SessionResult::Cancelled
                        } else {
                            SessionResult::Completed(self.last_answer.clone())
                        };
                        if self.finalize_when_idle(result) {
                            return;
                        }
                        continue;
                    }
                }
                tokio::select! {
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
                }
            }
            let (prompt, consumed) = self.take_prompt_batch();
            self.status(SessionStatus::Busy);
            if !prompt.is_empty()
                && let Err(error) = self.commit_user_batch(prompt, consumed).await
            {
                self.terminate(SessionResult::Failed(format!("{error:#}")), Vec::new())
                    .await;
                return;
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
                        if !self.has_work() && self.finish_cancelled_or_idle() {
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
                            if !self.has_work() && self.finish_cancelled_or_idle() {
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
                            if !self.has_work() && self.finish_cancelled_or_idle() {
                                return;
                            }
                            break 'turn;
                        }
                        WaitOutcome::Closed => {
                            self.terminate(SessionResult::Closed, waited.pending).await;
                            return;
                        }
                    };
                    let entry = tool_entry(&call, &result);
                    if let Err(error) = self.commit(entry).await {
                        self.terminate(SessionResult::Failed(format!("{error:#}")), waited.pending)
                            .await;
                        return;
                    }
                    self.agent.after_tool_entry(&call, &result);
                    self.agent.emit_event(AgentEvent::ToolResult {
                        is_error: result.is_err(),
                        content: result.unwrap_or_else(|error| error),
                    });
                    self.intake_after_operation(waited.pending);
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
            message: Message::User { content },
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
    }
}

fn tool_entry(call: &ToolCall, result: &Result<String, String>) -> SessionEntry {
    Message::Tool {
        call_id: call.id.clone(),
        name: call.name.clone(),
        content: result.as_ref().unwrap_or_else(|e| e).clone(),
        is_error: result.is_err(),
        synthetic: false,
    }
    .into()
}

#[cfg(test)]
#[path = "runner_tests.rs"]
mod tests;
