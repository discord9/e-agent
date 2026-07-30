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
}
impl Shared {
    fn emit(&mut self, event: AgentEvent) {
        self.log.push(event.clone());
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
        let _ = self.commands.send(SessionCommand::Prompt(prompt.into()));
    }
    pub fn cancel(&self) {
        let _ = self.commands.send(SessionCommand::Cancel);
    }
    pub fn compact(&self) {
        let _ = self.commands.send(SessionCommand::Compact);
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
        self.task.take().expect("task already joined").await
    }
}

pub struct SessionRunner {
    agent: Agent,
    store: SessionStore,
    root: PathBuf,
    session: String,
    shared: Arc<Mutex<Shared>>,
    commands: mpsc::UnboundedReceiver<SessionCommand>,
    pending: VecDeque<String>,
    compact_pending: bool,
    policy: IdlePolicy,
    last_answer: Option<String>,
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
        }));
        let handler_shared = shared.clone();
        agent.set_event_handler(Box::new(move |event| {
            handler_shared.lock().unwrap().emit(event)
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
                compact_pending: false,
                policy,
                last_answer: None,
            },
            handle,
        )
    }

    pub fn start(mut self, initial_prompt: Option<String>) -> SessionTask {
        if let Some(prompt) = initial_prompt {
            self.pending.push_back(prompt);
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
            _ => None,
        };
        self.agent.apply_entry(entry);
        if let Some(event) = event {
            self.shared.lock().unwrap().emit(event);
        }
        Ok(())
    }
    fn status(&self, status: SessionStatus) {
        self.shared.lock().unwrap().status.send_replace(status);
    }
    fn failed(&self, error: anyhow::Error) {
        let text = format!("{error:#}");
        let mut shared = self.shared.lock().unwrap();
        shared.emit(AgentEvent::Error(text.clone()));
        shared
            .status
            .send_replace(SessionStatus::Finished(SessionResult::Failed(text)));
    }
    fn closed(&self) {
        self.status(SessionStatus::Finished(SessionResult::Closed));
    }
    fn queue(&mut self, command: SessionCommand) -> bool {
        match command {
            SessionCommand::Prompt(p) => self.pending.push_back(p),
            SessionCommand::Compact => self.compact_pending = true,
            SessionCommand::Cancel => return true,
        }
        false
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

    async fn run(&mut self) {
        loop {
            match self.commit_backgrounds().await {
                Ok(true) if self.pending.is_empty() => self.pending.push_back(String::new()),
                Ok(_) => {}
                Err(error) => {
                    self.failed(error);
                    return;
                }
            }
            if self.compact_pending {
                self.compact_pending = false;
                self.status(SessionStatus::Compacting);
                let waited = await_compaction(&mut self.agent, &mut self.commands).await;
                for command in waited.pending {
                    self.queue(command);
                }
                match waited.outcome {
                    WaitOutcome::Completed(Ok(out)) => {
                        let usage = out.usage;
                        if let Err(error) = self.commit(out.entry).await {
                            self.failed(error);
                            return;
                        }
                        self.agent.apply_usage(usage, false);
                    }
                    WaitOutcome::Completed(Err(error)) => {
                        self.failed(error);
                        return;
                    }
                    WaitOutcome::Cancelled => {
                        self.shared
                            .lock()
                            .unwrap()
                            .emit(AgentEvent::Notice("compaction cancelled".into()));
                        self.status(SessionStatus::Idle);
                        continue;
                    }
                    WaitOutcome::Closed => {
                        self.closed();
                        return;
                    }
                }
            }
            if self.pending.is_empty() {
                self.status(SessionStatus::Idle);
                if self.policy == IdlePolicy::FinishWhenIdle {
                    self.status(SessionStatus::Finished(SessionResult::Completed(
                        self.last_answer.clone(),
                    )));
                    return;
                }
                tokio::select! {
                    command = self.commands.recv() => match command {
                        Some(command) => {
                            self.queue(command);
                            while let Ok(command) = self.commands.try_recv() { self.queue(command); }
                            continue;
                        }
                        None => { self.status(SessionStatus::Finished(SessionResult::Closed)); return; }
                    },
                    ready = self.agent.wait_background_ready() => {
                        if ready { continue; }
                        self.status(SessionStatus::Finished(SessionResult::Closed)); return;
                    }
                }
            }
            let prompt = self.pending.drain(..).collect::<Vec<_>>().join("\n\n");
            self.status(SessionStatus::Busy);
            if !prompt.is_empty()
                && let Err(error) = self.commit(Message::User { content: prompt }.into()).await
            {
                self.failed(error);
                return;
            }
            let specs = self.agent.tool_specs();
            'turn: loop {
                let waited = await_round(&mut self.agent, &specs, &mut self.commands).await;
                for command in waited.pending {
                    self.queue(command);
                }
                let round = match waited.outcome {
                    WaitOutcome::Completed(Ok(round)) => round,
                    WaitOutcome::Completed(Err(error)) => {
                        self.failed(error);
                        return;
                    }
                    WaitOutcome::Cancelled => {
                        self.shared
                            .lock()
                            .unwrap()
                            .emit(AgentEvent::Notice("turn cancelled".into()));
                        self.status(SessionStatus::Idle);
                        break 'turn;
                    }
                    WaitOutcome::Closed => {
                        self.closed();
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
                    self.failed(error);
                    return;
                }
                self.agent.apply_usage(usage, true);
                if self.agent.take_auto_compact_request() {
                    self.agent
                        .emit_event(AgentEvent::Notice("──── auto-compacting… ────".into()));
                    let waited = await_compaction(&mut self.agent, &mut self.commands).await;
                    for command in waited.pending {
                        self.queue(command);
                    }
                    match waited.outcome {
                        WaitOutcome::Completed(Ok(out)) => {
                            let usage = out.usage;
                            if let Err(error) = self.commit(out.entry).await {
                                self.failed(error);
                                return;
                            }
                            self.agent.apply_usage(usage, false);
                            self.agent
                                .emit_event(AgentEvent::Notice("──── auto-compaction ────".into()));
                        }
                        WaitOutcome::Completed(Err(error)) => {
                            self.agent.reset_auto_compact_request();
                            self.agent.emit_event(AgentEvent::Notice(format!(
                                "auto-compaction error: {error:#}"
                            )));
                        }
                        WaitOutcome::Cancelled => {
                            self.agent.reset_auto_compact_request();
                            self.agent
                                .emit_event(AgentEvent::Notice("auto-compaction cancelled".into()));
                            self.status(SessionStatus::Idle);
                            break 'turn;
                        }
                        WaitOutcome::Closed => {
                            self.closed();
                            return;
                        }
                    }
                }
                if !streamed && let Some(text) = content.filter(|text| !text.is_empty()) {
                    self.agent.emit_event(AgentEvent::AssistantText(text));
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
                    for command in waited.pending {
                        self.queue(command);
                    }
                    let result = match waited.outcome {
                        WaitOutcome::Completed(result) => result,
                        WaitOutcome::Cancelled => {
                            self.shared
                                .lock()
                                .unwrap()
                                .emit(AgentEvent::Notice("turn cancelled".into()));
                            self.status(SessionStatus::Idle);
                            break 'turn;
                        }
                        WaitOutcome::Closed => {
                            self.closed();
                            return;
                        }
                    };
                    let entry = tool_entry(&call, &result);
                    if let Err(error) = self.commit(entry).await {
                        self.failed(error);
                        return;
                    }
                    self.agent.after_tool_entry(&call, &result);
                    self.agent.emit_event(AgentEvent::ToolResult {
                        is_error: result.is_err(),
                        content: result.unwrap_or_else(|error| error),
                    });
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
    }
    .into()
}
