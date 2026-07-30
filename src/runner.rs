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
            let _ = self.events.send(event);
        } else {
            self.emit(event);
        }
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
            shared.emit(AgentEvent::UserPrompt(prompt));
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
    Prompt { text: String, projected: bool },
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
                projected: false,
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
            _ => None,
        };
        self.agent.apply_entry(entry);
        if let Some(event) = event {
            self.shared.lock().unwrap().emit(event);
        }
        Ok(())
    }
    async fn commit_user_batch(
        &mut self,
        content: String,
        unprojected: Vec<String>,
    ) -> anyhow::Result<()> {
        let entry: SessionEntry = Message::User { content }.into();
        self.store
            .append(&self.root, &self.session, std::slice::from_ref(&entry))
            .await?;
        self.agent.apply_entry(entry);
        let mut shared = self.shared.lock().unwrap();
        for prompt in unprojected {
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
                        let (prompt, unprojected) = self.take_prompt_batch();
                        if !prompt.is_empty()
                            && let Err(error) = self.commit_user_batch(prompt, unprojected).await
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
                    projected: true,
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

    fn take_prompt_batch(&mut self) -> (String, Vec<String>) {
        let mut prompts = Vec::new();
        let mut unprojected = Vec::new();
        while matches!(self.pending.front(), Some(PendingCommand::Prompt { .. })) {
            let Some(PendingCommand::Prompt { text, projected }) = self.pending.pop_front() else {
                unreachable!()
            };
            if !projected && !text.is_empty() {
                unprojected.push(text.clone());
            }
            prompts.push(text);
        }
        (prompts.join("\n\n"), unprojected)
    }

    fn finish_cancelled_or_idle(&mut self) -> bool {
        if self.policy == IdlePolicy::FinishWhenIdle {
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
                        projected: false,
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
                    if self.policy == IdlePolicy::FinishWhenIdle {
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
            let (prompt, unprojected) = self.take_prompt_batch();
            self.status(SessionStatus::Busy);
            if !prompt.is_empty()
                && let Err(error) = self.commit_user_batch(prompt, unprojected).await
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
                        self.terminate(SessionResult::Failed(format!("{error:#}")), waited.pending)
                            .await;
                        return;
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
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AssistantMessage, Model, ModelDeltaKind, Tool, Usage};
    use async_trait::async_trait;
    use serde_json::Value;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::sync::Notify;

    struct ControlledModel {
        replies: VecDeque<anyhow::Result<String>>,
        block_first: bool,
        entered: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait]
    impl Model for ControlledModel {
        async fn complete(
            &mut self,
            _: &[Message],
            _: &[ToolSpec],
            mut on_delta: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
        ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
            let reply = self.replies.pop_front().expect("unexpected model call");
            if reply.is_ok()
                && let Some(callback) = &mut on_delta
            {
                callback(ModelDeltaKind::Reasoning, "thinking");
                callback(ModelDeltaKind::Content, "streamed");
            }
            if self.block_first {
                self.block_first = false;
                self.entered.notify_one();
                self.release.notified().await;
            }
            let content = reply?;
            Ok((
                AssistantMessage {
                    content: Some(content),
                    tool_calls: Vec::new(),
                    reasoning: None,
                },
                None,
            ))
        }
    }

    struct DropProbeModel {
        entered: Arc<Notify>,
        release: Arc<Notify>,
        dropped: Arc<Notify>,
        side_effects: Arc<AtomicUsize>,
    }

    struct DropProbe(Arc<Notify>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }

    #[async_trait]
    impl Model for DropProbeModel {
        async fn complete(
            &mut self,
            _: &[Message],
            _: &[ToolSpec],
            _: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
        ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
            let _probe = DropProbe(self.dropped.clone());
            self.entered.notify_one();
            self.release.notified().await;
            self.side_effects.fetch_add(1, Ordering::SeqCst);
            Ok((
                AssistantMessage {
                    content: Some("too late".into()),
                    tool_calls: Vec::new(),
                    reasoning: None,
                },
                None,
            ))
        }
    }

    struct ScriptedAssistantModel {
        replies: VecDeque<AssistantMessage>,
    }

    #[async_trait]
    impl Model for ScriptedAssistantModel {
        async fn complete(
            &mut self,
            _: &[Message],
            _: &[ToolSpec],
            _: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
        ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
            Ok((
                self.replies.pop_front().expect("unexpected model call"),
                None,
            ))
        }
    }

    struct CompletingWithCancelTool {
        commands: Arc<Mutex<Option<mpsc::UnboundedSender<SessionCommand>>>>,
    }

    #[async_trait]
    impl Tool for CompletingWithCancelTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "cancel_tool".into(),
                description: "test only".into(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }
        async fn execute(&self, _: Value) -> Result<String, String> {
            self.commands
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .send(SessionCommand::Cancel)
                .unwrap();
            Ok("completed tool result".into())
        }
    }

    struct CompletingWithCancelModel {
        reply: Option<String>,
        commands: Arc<Mutex<Option<mpsc::UnboundedSender<SessionCommand>>>>,
    }

    #[async_trait]
    impl Model for CompletingWithCancelModel {
        async fn complete(
            &mut self,
            _: &[Message],
            _: &[ToolSpec],
            _: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
        ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
            self.commands
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .send(SessionCommand::Cancel)
                .unwrap();
            Ok((
                AssistantMessage {
                    content: self.reply.take(),
                    tool_calls: Vec::new(),
                    reasoning: None,
                },
                None,
            ))
        }
    }

    fn history_for_compaction(agent: &mut Agent) {
        agent.restore_history(vec![
            Message::User {
                content: "old question".into(),
            }
            .into(),
            Message::Assistant(AssistantMessage {
                content: Some("old answer".into()),
                tool_calls: Vec::new(),
                reasoning: None,
            })
            .into(),
            Message::User {
                content: "current question".into(),
            }
            .into(),
        ]);
    }

    async fn wait_for_status(
        status: &mut watch::Receiver<SessionStatus>,
        expected: impl Fn(&SessionStatus) -> bool,
    ) -> SessionStatus {
        loop {
            let value = status.borrow().clone();
            if expected(&value) {
                return value;
            }
            status.changed().await.unwrap();
        }
    }

    struct KeepAliveTool {
        sender: Option<mpsc::UnboundedSender<AgentEvent>>,
    }

    #[async_trait]
    impl Tool for KeepAliveTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "keep_alive".into(),
                description: "test only".into(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }
        async fn execute(&self, _: Value) -> Result<String, String> {
            Ok(String::new())
        }
        fn set_event_sender(&mut self, sender: mpsc::UnboundedSender<AgentEvent>) {
            self.sender = Some(sender);
        }
    }

    fn controlled(
        replies: Vec<anyhow::Result<String>>,
        block_first: bool,
    ) -> (Agent, Arc<Notify>, Arc<Notify>) {
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let model = ControlledModel {
            replies: replies.into(),
            block_first,
            entered: entered.clone(),
            release: release.clone(),
        };
        (
            Agent::new(
                Box::new(model),
                vec![Box::new(KeepAliveTool { sender: None })],
            ),
            entered,
            release,
        )
    }

    #[tokio::test]
    async fn aborting_join_waiter_still_aborts_inner_runner() {
        let temp = tempfile::tempdir().unwrap();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let dropped = Arc::new(Notify::new());
        let side_effects = Arc::new(AtomicUsize::new(0));
        let agent = Agent::new(
            Box::new(DropProbeModel {
                entered: entered.clone(),
                release: release.clone(),
                dropped: dropped.clone(),
                side_effects: side_effects.clone(),
            }),
            vec![],
        );
        let (runner, _handle) = SessionRunner::new(
            agent,
            SessionStore::Jsonl,
            temp.path().into(),
            "abort-join".into(),
            IdlePolicy::FinishWhenIdle,
        );
        let task = runner.start(Some("start".into()));
        let waiter = tokio::spawn(task.join());

        entered.notified().await;
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        dropped.notified().await;
        release.notify_one();
        tokio::task::yield_now().await;
        assert_eq!(side_effects.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn compaction_deltas_are_live_only_and_success_has_one_projection() {
        let temp = tempfile::tempdir().unwrap();
        let (mut agent, entered, release) = controlled(vec![Ok("summary".into())], true);
        history_for_compaction(&mut agent);
        let (runner, handle) = SessionRunner::new(
            agent,
            SessionStore::Jsonl,
            temp.path().into(),
            "success".into(),
            IdlePolicy::WaitForInput,
        );
        let (_, mut live, status) = handle.attach();
        handle.compact();
        let task = runner.start(None);
        entered.notified().await;
        assert!(matches!(*status.borrow(), SessionStatus::Compacting));
        let first = live.recv().await.unwrap();
        let second = live.recv().await.unwrap();
        assert!(matches!(first, AgentEvent::ReasoningDelta(_)));
        assert!(matches!(second, AgentEvent::AssistantDelta(_)));
        assert!(!handle.snapshot().iter().any(|event| matches!(
            event,
            AgentEvent::AssistantDelta(_) | AgentEvent::ReasoningDelta(_)
        )));

        release.notify_one();
        assert!(
            matches!(live.recv().await.unwrap(), AgentEvent::Notice(text) if text == "compacted: summary")
        );
        let snapshot = handle.snapshot();
        assert_eq!(
            snapshot
                .iter()
                .filter(|event| matches!(event, AgentEvent::Notice(text) if text == "compacted: summary"))
                .count(),
            1
        );
        assert!(!snapshot.iter().any(|event| matches!(
            event,
            AgentEvent::AssistantDelta(_) | AgentEvent::ReasoningDelta(_)
        )));
        let loaded = SessionStore::Jsonl
            .load(temp.path(), "success")
            .await
            .unwrap();
        assert_eq!(
            loaded
                .entries
                .iter()
                .filter(|entry| matches!(entry, SessionEntry::Compaction { .. }))
                .count(),
            1
        );
        drop(handle);
        drop(task);
    }

    #[tokio::test]
    async fn failed_compaction_has_no_projection_or_persisted_entry() {
        let temp = tempfile::tempdir().unwrap();
        let (mut agent, _, _) = controlled(vec![Err(anyhow::anyhow!("provider failed"))], false);
        history_for_compaction(&mut agent);
        let (runner, handle) = SessionRunner::new(
            agent,
            SessionStore::Jsonl,
            temp.path().into(),
            "failure".into(),
            IdlePolicy::WaitForInput,
        );
        let (_, mut live, _) = handle.attach();
        handle.compact();
        let task = runner.start(None);
        loop {
            if matches!(live.recv().await.unwrap(), AgentEvent::Error(text) if text.contains("provider failed"))
            {
                break;
            }
        }
        assert!(!handle.snapshot().iter().any(
            |event| matches!(event, AgentEvent::Notice(text) if text.starts_with("compacted:"))
        ));
        let loaded = SessionStore::Jsonl
            .load(temp.path(), "failure")
            .await
            .unwrap();
        assert!(
            !loaded
                .entries
                .iter()
                .any(|entry| matches!(entry, SessionEntry::Compaction { .. }))
        );
        drop(handle);
        drop(task);
    }

    #[tokio::test]
    async fn empty_manual_compaction_returns_idle_and_accepts_a_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let (agent, _, _) = controlled(vec![Ok("answer".into())], false);
        let (runner, handle) = SessionRunner::new(
            agent,
            SessionStore::Jsonl,
            temp.path().into(),
            "empty".into(),
            IdlePolicy::WaitForInput,
        );
        let (_, mut live, mut status) = handle.attach();
        handle.compact();
        let task = runner.start(None);
        loop {
            if matches!(live.recv().await.unwrap(), AgentEvent::Error(text) if text.contains("nothing to compact"))
            {
                break;
            }
        }
        assert!(handle.snapshot().iter().any(
            |event| matches!(event, AgentEvent::Error(text) if text.contains("nothing to compact"))
        ));
        handle.prompt("still alive");
        loop {
            if matches!(live.recv().await.unwrap(), AgentEvent::AssistantDelta(text) if text == "streamed")
            {
                break;
            }
        }
        wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;
        assert!(
            handle.snapshot().iter().any(
                |event| matches!(event, AgentEvent::UserPrompt(text) if text == "still alive")
            )
        );
        drop(handle);
        drop(task);
    }

    #[tokio::test]
    async fn finish_when_idle_drains_prompt_and_compact_queued_at_completion() {
        let temp = tempfile::tempdir().unwrap();
        let (mut agent, entered, release) = controlled(
            vec![
                Ok("first".into()),
                Ok("second".into()),
                Ok("summary".into()),
            ],
            true,
        );
        history_for_compaction(&mut agent);
        let (runner, handle) = SessionRunner::new(
            agent,
            SessionStore::Jsonl,
            temp.path().into(),
            "race".into(),
            IdlePolicy::FinishWhenIdle,
        );
        let task = runner.start(Some("first prompt".into()));
        entered.notified().await;
        handle.prompt("queued prompt");
        handle.compact();
        release.notify_one();
        let mut status = handle.status();
        let result =
            wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
        assert_eq!(
            result,
            SessionStatus::Finished(SessionResult::Completed(Some("second".into())))
        );
        let snapshot = handle.snapshot();
        assert!(
            snapshot.iter().any(
                |event| matches!(event, AgentEvent::UserPrompt(text) if text == "queued prompt")
            )
        );
        assert_eq!(
            snapshot
                .iter()
                .filter(|event| matches!(event, AgentEvent::Notice(text) if text == "compacted: summary"))
                .count(),
            1
        );
        task.join().await.unwrap();
    }

    #[tokio::test]
    async fn prompt_racing_finalization_is_consumed_and_persisted() {
        let temp = tempfile::tempdir().unwrap();
        let (agent, _, _) = controlled(vec![Ok("first".into()), Ok("second".into())], false);
        let (mut runner, handle) = SessionRunner::new(
            agent,
            SessionStore::Jsonl,
            temp.path().into(),
            "finalize-race".into(),
            IdlePolicy::FinishWhenIdle,
        );
        let racing = handle.clone();
        runner.before_finalize = Some(Box::new(move || racing.prompt("at finalization")));
        let task = runner.start(Some("initial".into()));
        let mut status = handle.status();
        wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;

        assert_eq!(
            handle
                .snapshot()
                .iter()
                .filter(|event| matches!(event, AgentEvent::UserPrompt(text) if text == "at finalization"))
                .count(),
            1
        );
        let loaded = SessionStore::Jsonl
            .load(temp.path(), "finalize-race")
            .await
            .unwrap();
        assert_eq!(
            loaded
                .entries
                .iter()
                .filter(|entry| matches!(
                    entry,
                    SessionEntry::Message { message: Message::User { content } }
                        if content == "at finalization"
                ))
                .count(),
            1
        );
        task.join().await.unwrap();
    }

    #[tokio::test]
    async fn prompt_accepted_before_round_failure_is_persisted_before_failure() {
        let temp = tempfile::tempdir().unwrap();
        let (agent, entered, release) =
            controlled(vec![Err(anyhow::anyhow!("round failed"))], true);
        let (runner, handle) = SessionRunner::new(
            agent,
            SessionStore::Jsonl,
            temp.path().into(),
            "failure-race".into(),
            IdlePolicy::FinishWhenIdle,
        );
        let task = runner.start(Some("initial".into()));
        entered.notified().await;
        handle.prompt("accepted before failure");
        release.notify_one();
        let mut status = handle.status();
        let result =
            wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
        assert!(matches!(
            result,
            SessionStatus::Finished(SessionResult::Failed(text)) if text.contains("round failed")
        ));
        let loaded = SessionStore::Jsonl
            .load(temp.path(), "failure-race")
            .await
            .unwrap();
        assert!(loaded.entries.iter().any(|entry| matches!(
            entry,
            SessionEntry::Message { message: Message::User { content } }
                if content == "accepted before failure"
        )));
        assert!(!loaded.entries.iter().any(|entry| matches!(
            entry,
            SessionEntry::Message {
                message: Message::Assistant(_)
            }
        )));
        task.join().await.unwrap();
    }

    #[tokio::test]
    async fn prompt_after_round_failure_has_no_projection_or_persistence() {
        let temp = tempfile::tempdir().unwrap();
        let (agent, _, _) = controlled(vec![Err(anyhow::anyhow!("round failed"))], false);
        let (runner, handle) = SessionRunner::new(
            agent,
            SessionStore::Jsonl,
            temp.path().into(),
            "failure-first".into(),
            IdlePolicy::FinishWhenIdle,
        );
        let task = runner.start(Some("initial".into()));
        let mut status = handle.status();
        wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
        let before = handle.snapshot();
        handle.prompt("too late");
        assert_eq!(handle.snapshot(), before);
        let loaded = SessionStore::Jsonl
            .load(temp.path(), "failure-first")
            .await
            .unwrap();
        assert!(!loaded.entries.iter().any(|entry| matches!(
            entry,
            SessionEntry::Message { message: Message::User { content } }
                if content == "too late"
        )));
        task.join().await.unwrap();
    }

    #[tokio::test]
    async fn prompt_after_finished_has_no_projection_or_persistence() {
        let temp = tempfile::tempdir().unwrap();
        let (agent, _, _) = controlled(vec![Ok("answer".into())], false);
        let (runner, handle) = SessionRunner::new(
            agent,
            SessionStore::Jsonl,
            temp.path().into(),
            "finished-prompt".into(),
            IdlePolicy::FinishWhenIdle,
        );
        let (_, mut live, mut status) = handle.attach();
        let task = runner.start(Some("initial".into()));
        wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
        while live.try_recv().is_ok() {}
        let before = handle.snapshot();

        handle.prompt("too late");

        assert_eq!(handle.snapshot(), before);
        assert!(live.try_recv().is_err());
        let loaded = SessionStore::Jsonl
            .load(temp.path(), "finished-prompt")
            .await
            .unwrap();
        assert!(!loaded.entries.iter().any(|entry| matches!(
            entry,
            SessionEntry::Message { message: Message::User { content } }
                if content == "too late"
        )));
        task.join().await.unwrap();
    }

    #[tokio::test]
    async fn queued_handle_prompt_is_projected_immediately_and_only_once() {
        let temp = tempfile::tempdir().unwrap();
        let (agent, entered, release) =
            controlled(vec![Ok("first".into()), Ok("second".into())], true);
        let (runner, handle) = SessionRunner::new(
            agent,
            SessionStore::Jsonl,
            temp.path().into(),
            "prompt-projection".into(),
            IdlePolicy::FinishWhenIdle,
        );
        let (_, mut live, _) = handle.attach();
        let task = runner.start(Some("initial".into()));
        entered.notified().await;
        handle.prompt("queued while busy");

        loop {
            if matches!(live.recv().await.unwrap(), AgentEvent::UserPrompt(text) if text == "queued while busy")
            {
                break;
            }
        }
        assert_eq!(
            handle
                .snapshot()
                .iter()
                .filter(|event| matches!(event, AgentEvent::UserPrompt(text) if text == "queued while busy"))
                .count(),
            1
        );

        release.notify_one();
        let mut status = handle.status();
        wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
        assert_eq!(
            handle
                .snapshot()
                .iter()
                .filter(|event| matches!(event, AgentEvent::UserPrompt(text) if text == "queued while busy"))
                .count(),
            1
        );
        let loaded = SessionStore::Jsonl
            .load(temp.path(), "prompt-projection")
            .await
            .unwrap();
        assert!(loaded.entries.iter().any(|entry| matches!(
            entry,
            SessionEntry::Message { message: Message::User { content } }
                if content == "queued while busy"
        )));
        task.join().await.unwrap();
    }

    #[tokio::test]
    async fn prompt_and_compact_pending_order_is_fifo() {
        for prompt_first in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            let replies = if prompt_first {
                vec![Ok("answer".into()), Ok("summary".into())]
            } else {
                vec![Ok("summary".into()), Ok("answer".into())]
            };
            let (mut agent, _, _) = controlled(replies, false);
            history_for_compaction(&mut agent);
            let session = if prompt_first {
                "prompt-compact"
            } else {
                "compact-prompt"
            };
            let (runner, handle) = SessionRunner::new(
                agent,
                SessionStore::Jsonl,
                temp.path().into(),
                session.into(),
                IdlePolicy::FinishWhenIdle,
            );
            if prompt_first {
                handle.prompt("queued prompt");
                handle.compact();
            } else {
                handle.compact();
                handle.prompt("queued prompt");
            }
            let task = runner.start(None);
            let mut status = handle.status();
            wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;

            let loaded = SessionStore::Jsonl
                .load(temp.path(), session)
                .await
                .unwrap();
            let prompt = loaded
                .entries
                .iter()
                .position(|entry| {
                    matches!(
                        entry,
                        SessionEntry::Message { message: Message::User { content } }
                            if content == "queued prompt"
                    )
                })
                .unwrap();
            let compact = loaded
                .entries
                .iter()
                .position(|entry| matches!(entry, SessionEntry::Compaction { summary, .. } if summary == "summary"))
                .unwrap();
            assert_eq!(prompt < compact, prompt_first);
            task.join().await.unwrap();
        }
    }

    #[tokio::test]
    async fn consecutive_compacts_are_not_folded() {
        let temp = tempfile::tempdir().unwrap();
        let (mut agent, _, _) = controlled(vec![Ok("summary".into())], false);
        history_for_compaction(&mut agent);
        let (runner, handle) = SessionRunner::new(
            agent,
            SessionStore::Jsonl,
            temp.path().into(),
            "two-compacts".into(),
            IdlePolicy::FinishWhenIdle,
        );
        handle.compact();
        handle.compact();
        let task = runner.start(None);
        let mut status = handle.status();
        wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;

        let snapshot = handle.snapshot();
        assert_eq!(
            snapshot
                .iter()
                .filter(|event| matches!(event, AgentEvent::Notice(text) if text == "compacted: summary"))
                .count(),
            1
        );
        assert_eq!(
            snapshot
                .iter()
                .filter(|event| matches!(event, AgentEvent::Error(text) if text.contains("nothing to compact")))
                .count(),
            1
        );
        task.join().await.unwrap();
    }

    #[tokio::test]
    async fn completed_tool_result_is_committed_before_stale_cancel() {
        let temp = tempfile::tempdir().unwrap();
        let commands = Arc::new(Mutex::new(None));
        let agent = Agent::new(
            Box::new(ScriptedAssistantModel {
                replies: vec![AssistantMessage {
                    content: None,
                    tool_calls: vec![ToolCall {
                        id: "call-1".into(),
                        name: "cancel_tool".into(),
                        arguments: "{}".into(),
                    }],
                    reasoning: None,
                }]
                .into(),
            }),
            vec![Box::new(CompletingWithCancelTool {
                commands: commands.clone(),
            })],
        );
        let (runner, handle) = SessionRunner::new(
            agent,
            SessionStore::Jsonl,
            temp.path().into(),
            "tool-cancel".into(),
            IdlePolicy::FinishWhenIdle,
        );
        *commands.lock().unwrap() = Some(handle.commands.clone());
        let task = runner.start(Some("prompt".into()));
        let mut status = handle.status();
        let result =
            wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
        assert_eq!(result, SessionStatus::Finished(SessionResult::Cancelled));
        let loaded = SessionStore::Jsonl
            .load(temp.path(), "tool-cancel")
            .await
            .unwrap();
        assert!(loaded.entries.iter().any(|entry| matches!(
            entry,
            SessionEntry::Message {
                message: Message::Tool { content, .. }
            } if content == "completed tool result"
        )));
        task.join().await.unwrap();
    }

    #[tokio::test]
    async fn in_flight_compaction_cancel_has_no_entry_or_projection() {
        let temp = tempfile::tempdir().unwrap();
        let (mut agent, entered, _) = controlled(vec![Ok("unused summary".into())], true);
        history_for_compaction(&mut agent);
        let (runner, handle) = SessionRunner::new(
            agent,
            SessionStore::Jsonl,
            temp.path().into(),
            "compact-in-flight-cancel".into(),
            IdlePolicy::FinishWhenIdle,
        );
        handle.compact();
        let task = runner.start(None);
        entered.notified().await;
        handle.cancel();
        let mut status = handle.status();
        let result =
            wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
        assert_eq!(result, SessionStatus::Finished(SessionResult::Cancelled));
        let loaded = SessionStore::Jsonl
            .load(temp.path(), "compact-in-flight-cancel")
            .await
            .unwrap();
        assert!(
            !loaded
                .entries
                .iter()
                .any(|entry| matches!(entry, SessionEntry::Compaction { .. }))
        );
        assert!(!handle.snapshot().iter().any(
            |event| matches!(event, AgentEvent::Notice(text) if text.starts_with("compacted:"))
        ));
        task.join().await.unwrap();
    }

    #[tokio::test]
    async fn completed_round_is_committed_before_stale_cancel() {
        let temp = tempfile::tempdir().unwrap();
        let commands = Arc::new(Mutex::new(None));
        let agent = Agent::new(
            Box::new(CompletingWithCancelModel {
                reply: Some("completed answer".into()),
                commands: commands.clone(),
            }),
            vec![],
        );
        let (runner, handle) = SessionRunner::new(
            agent,
            SessionStore::Jsonl,
            temp.path().into(),
            "round-cancel".into(),
            IdlePolicy::FinishWhenIdle,
        );
        *commands.lock().unwrap() = Some(handle.commands.clone());
        let task = runner.start(Some("prompt".into()));
        let mut status = handle.status();
        let result =
            wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
        assert_eq!(result, SessionStatus::Finished(SessionResult::Cancelled));
        let loaded = SessionStore::Jsonl
            .load(temp.path(), "round-cancel")
            .await
            .unwrap();
        assert!(loaded.entries.iter().any(|entry| matches!(
            entry,
            SessionEntry::Message { message: Message::Assistant(message) }
                if message.content.as_deref() == Some("completed answer")
        )));
        task.join().await.unwrap();
    }

    #[tokio::test]
    async fn completed_compaction_is_committed_before_stale_cancel() {
        let temp = tempfile::tempdir().unwrap();
        let commands = Arc::new(Mutex::new(None));
        let mut agent = Agent::new(
            Box::new(CompletingWithCancelModel {
                reply: Some("completed summary".into()),
                commands: commands.clone(),
            }),
            vec![],
        );
        history_for_compaction(&mut agent);
        let (runner, handle) = SessionRunner::new(
            agent,
            SessionStore::Jsonl,
            temp.path().into(),
            "compact-cancel".into(),
            IdlePolicy::FinishWhenIdle,
        );
        *commands.lock().unwrap() = Some(handle.commands.clone());
        handle.compact();
        let task = runner.start(None);
        let mut status = handle.status();
        let result =
            wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
        assert_eq!(result, SessionStatus::Finished(SessionResult::Cancelled));
        let loaded = SessionStore::Jsonl
            .load(temp.path(), "compact-cancel")
            .await
            .unwrap();
        assert_eq!(
            loaded
                .entries
                .iter()
                .filter(|entry| matches!(entry, SessionEntry::Compaction { summary, .. } if summary == "completed summary"))
                .count(),
            1
        );
        assert_eq!(
            handle
                .snapshot()
                .iter()
                .filter(|event| matches!(event, AgentEvent::Notice(text) if text == "compacted: completed summary"))
                .count(),
            1
        );
        task.join().await.unwrap();
    }

    #[tokio::test]
    async fn cancel_without_queued_work_returns_cancelled() {
        let temp = tempfile::tempdir().unwrap();
        let (agent, entered, _) = controlled(vec![Ok("unused".into())], true);
        let (runner, handle) = SessionRunner::new(
            agent,
            SessionStore::Jsonl,
            temp.path().into(),
            "cancel".into(),
            IdlePolicy::FinishWhenIdle,
        );
        let task = runner.start(Some("prompt".into()));
        entered.notified().await;
        handle.cancel();
        let mut status = handle.status();
        let result =
            wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
        assert_eq!(result, SessionStatus::Finished(SessionResult::Cancelled));
        assert!(!handle.snapshot().iter().any(
            |event| matches!(event, AgentEvent::Notice(text) if text.starts_with("compacted:"))
        ));
        task.join().await.unwrap();
    }
}
