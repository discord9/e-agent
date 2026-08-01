use super::*;
use crate::agent::{AssistantMessage, Model, ModelDeltaKind, Tool, Usage, repair_tool_pairs};
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

/// Mock model that records its own name on every call, so a test can prove
/// which model served which turn (runtime `/model` switch).
struct NamedRecordingModel {
    name: String,
    calls: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Model for NamedRecordingModel {
    async fn complete(
        &mut self,
        _: &[Message],
        _: &[ToolSpec],
        _: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        self.calls.lock().unwrap().push(self.name.clone());
        Ok((
            AssistantMessage {
                content: Some(format!("from {}", self.name)),
                tool_calls: Vec::new(),
                reasoning: None,
            },
            None,
        ))
    }
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

/// Scripted model for the recoverable-failure tests: the first call can be
/// blocked (so a test can queue commands mid-call) and fail, later calls
/// succeed. Never streams deltas, so a successful turn projects a plain
/// `AssistantText` event.
struct RecoveringModel {
    replies: VecDeque<anyhow::Result<AssistantMessage>>,
    block_first: bool,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl Model for RecoveringModel {
    async fn complete(
        &mut self,
        _: &[Message],
        _: &[ToolSpec],
        _: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        let reply = self.replies.pop_front().expect("unexpected model call");
        if self.block_first {
            self.block_first = false;
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok((reply?, None))
    }
}

#[test]
fn entry_event_projects_forked_from_as_notice() {
    let event = entry_event(&SessionEntry::ForkedFrom {
        source: "src-123".into(),
        at: 4,
        event_time: Some(1_700_000_000_000_000),
        seq: Some(3),
    });
    assert!(
        matches!(event, Some(AgentEvent::Notice(text)) if text == "forked from src-123 at entry 4"),
        "forked_from must project to a dim Notice line"
    );
    // Provenance fields never leak into the projection.
    let event = entry_event(&SessionEntry::ForkedFrom {
        source: "src-123".into(),
        at: 4,
        event_time: None,
        seq: None,
    });
    assert!(
        matches!(event, Some(AgentEvent::Notice(text)) if text == "forked from src-123 at entry 4")
    );
}

fn recovering_agent(
    replies: Vec<anyhow::Result<AssistantMessage>>,
    block_first: bool,
) -> (Agent, Arc<Notify>, Arc<Notify>) {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let model = RecoveringModel {
        replies: replies.into(),
        block_first,
        entered: entered.clone(),
        release: release.clone(),
    };
    (
        // The keep-alive tool holds a clone of the agent's background
        // completion sender, keeping that channel open. Without a tool the
        // sender is dropped in Agent::new, so the runner's idle
        // `wait_background_ready()` returns false immediately and the
        // session terminates with Finished(Closed) instead of staying Idle.
        Agent::new(
            Box::new(model),
            vec![Box::new(KeepAliveTool { sender: None })],
        ),
        entered,
        release,
    )
}

/// Scripted replies with per-call usage, capturing every context the model
/// is called with, so a test can inspect the derived context of later calls.
struct ScriptedContextCaptureModel {
    replies: VecDeque<(AssistantMessage, Option<Usage>)>,
    calls: Arc<Mutex<Vec<Vec<Message>>>>,
}

#[async_trait]
impl Model for ScriptedContextCaptureModel {
    async fn complete(
        &mut self,
        messages: &[Message],
        _: &[ToolSpec],
        _: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        self.calls.lock().unwrap().push(messages.to_vec());
        Ok(self.replies.pop_front().expect("unexpected model call"))
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
            images: vec![],
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
            images: vec![],
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
            .filter(
                |event| matches!(event, AgentEvent::Notice(text) if text == "compacted: summary")
            )
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
    assert!(
        !handle.snapshot().iter().any(
            |event| matches!(event, AgentEvent::Notice(text) if text.starts_with("compacted:"))
        )
    );
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
        handle
            .snapshot()
            .iter()
            .any(|event| matches!(event, AgentEvent::UserPrompt(text) if text == "still alive"))
    );
    drop(handle);
    drop(task);
}

#[tokio::test]
async fn switch_model_applies_to_the_next_turn() {
    let temp = tempfile::tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let (runner, handle) = SessionRunner::new(
        Agent::new(
            Box::new(NamedRecordingModel {
                name: "model-a".into(),
                calls: calls.clone(),
            }),
            // Keep the background channel open so the runner stays Idle
            // between turns (see `controlled`).
            vec![Box::new(KeepAliveTool { sender: None })],
        ),
        SessionStore::Jsonl,
        temp.path().into(),
        "switch-model".into(),
        IdlePolicy::WaitForInput,
    );
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(Some("first".into()));
    // Turn 1 runs on the startup model.
    loop {
        if matches!(
            live.recv().await.unwrap(),
            AgentEvent::AssistantText(text) if text == "from model-a"
        ) {
            break;
        }
    }
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;

    // Runtime switch, then turn 2 must run on the new model.
    handle.switch_model(Box::new(NamedRecordingModel {
        name: "model-b".into(),
        calls: calls.clone(),
    }));
    handle.prompt("second");
    loop {
        if matches!(
            live.recv().await.unwrap(),
            AgentEvent::AssistantText(text) if text == "from model-b"
        ) {
            break;
        }
    }
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;
    assert_eq!(
        *calls.lock().unwrap(),
        vec!["model-a".to_string(), "model-b".to_string()]
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
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert_eq!(
        result,
        SessionStatus::Finished(SessionResult::Completed(Some("second".into())))
    );
    let snapshot = handle.snapshot();
    assert!(
        snapshot
            .iter()
            .any(|event| matches!(event, AgentEvent::UserPrompt(text) if text == "queued prompt"))
    );
    assert_eq!(
        snapshot
            .iter()
            .filter(
                |event| matches!(event, AgentEvent::Notice(text) if text == "compacted: summary")
            )
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
            .filter(
                |event| matches!(event, AgentEvent::UserPrompt(text) if text == "at finalization")
            )
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
                SessionEntry::Message { message: Message::User { content, .. } }
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
    let (agent, entered, release) = controlled(vec![Err(anyhow::anyhow!("round failed"))], true);
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
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
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
        SessionEntry::Message { message: Message::User { content, .. } }
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
        SessionEntry::Message { message: Message::User { content, .. } }
            if content == "too late"
    )));
    task.join().await.unwrap();
}

#[tokio::test]
async fn model_call_failure_returns_to_idle_and_recovers() {
    let temp = tempfile::tempdir().unwrap();
    let (agent, entered, release) = recovering_agent(
        vec![
            Err(anyhow::anyhow!("model boom")),
            Ok(AssistantMessage {
                content: Some("recovered".into()),
                tool_calls: Vec::new(),
                reasoning: None,
            }),
        ],
        true,
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "recoverable-failure".into(),
        IdlePolicy::WaitForInput,
    );
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(Some("initial".into()));
    entered.notified().await;
    release.notify_one();

    // The failed round surfaces as an Error event and the runner returns to
    // Idle instead of terminating.
    loop {
        if matches!(
            live.recv().await.unwrap(),
            AgentEvent::Error(text)
                if text.contains("model call failed") && text.contains("model boom")
        ) {
            break;
        }
    }
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;
    assert!(!matches!(*status.borrow(), SessionStatus::Finished(_)));
    assert!(handle.snapshot().iter().any(|event| matches!(
        event,
        AgentEvent::Error(text) if text.contains("model call failed")
    )));

    // The failed round committed nothing: only the initial user prompt is on disk.
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "recoverable-failure")
        .await
        .unwrap();
    assert_eq!(loaded.entries.len(), 1);
    assert!(matches!(
        &loaded.entries[0],
        SessionEntry::Message {
            message: Message::User { content, .. }
        } if content == "initial"
    ));

    // A fresh prompt opens a new turn that succeeds.
    handle.prompt("retry prompt");
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "recovered" => break,
            AgentEvent::Error(_) => panic!("second turn failed"),
            _ => {}
        }
    }
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;
    assert!(handle.snapshot().iter().any(|event| matches!(
        event,
        AgentEvent::UserPrompt(text) if text == "retry prompt"
    )));
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "recoverable-failure")
        .await
        .unwrap();
    assert!(loaded.entries.iter().any(|entry| matches!(
        entry,
        SessionEntry::Message {
            message: Message::Assistant(message)
        } if message.content.as_deref() == Some("recovered")
    )));
    assert!(!loaded.entries.iter().any(|entry| matches!(
        entry,
        SessionEntry::Message {
            message: Message::Tool { .. }
        }
    )));
    drop(handle);
    drop(task);
}

#[tokio::test]
async fn prompt_queued_during_failed_model_call_runs_next_turn() {
    let temp = tempfile::tempdir().unwrap();
    let (agent, entered, release) = recovering_agent(
        vec![
            Err(anyhow::anyhow!("model boom")),
            Ok(AssistantMessage {
                content: Some("recovered".into()),
                tool_calls: Vec::new(),
                reasoning: None,
            }),
        ],
        true,
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "queued-during-failure".into(),
        IdlePolicy::WaitForInput,
    );
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(Some("initial".into()));
    entered.notified().await;
    handle.prompt("queued during failure");
    assert!(handle.snapshot().iter().any(|event| matches!(
        event,
        AgentEvent::PromptQueued(text) if text == "queued during failure"
    )));
    release.notify_one();

    // Failure surfaces as an Error event, then the queued prompt automatically
    // opens a new turn that succeeds.
    loop {
        if matches!(
            live.recv().await.unwrap(),
            AgentEvent::Error(text) if text.contains("model call failed")
        ) {
            break;
        }
    }
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::UserPrompt(text) if text == "queued during failure" => break,
            AgentEvent::Error(_) => panic!("second turn failed"),
            _ => {}
        }
    }
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "recovered" => break,
            AgentEvent::Error(_) => panic!("second turn failed"),
            _ => {}
        }
    }
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;
    assert!(!matches!(*status.borrow(), SessionStatus::Finished(_)));

    let loaded = SessionStore::Jsonl
        .load(temp.path(), "queued-during-failure")
        .await
        .unwrap();
    assert!(loaded.entries.iter().any(|entry| matches!(
        entry,
        SessionEntry::Message {
            message: Message::User { content, .. }
        } if content == "queued during failure"
    )));
    assert!(loaded.entries.iter().any(|entry| matches!(
        entry,
        SessionEntry::Message {
            message: Message::Assistant(message)
        } if message.content.as_deref() == Some("recovered")
    )));
    assert!(!loaded.entries.iter().any(|entry| matches!(
        entry,
        SessionEntry::Message {
            message: Message::Tool { .. }
        }
    )));
    drop(handle);
    drop(task);
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
        SessionEntry::Message { message: Message::User { content, .. } }
            if content == "too late"
    )));
    task.join().await.unwrap();
}

#[tokio::test]
async fn queued_handle_prompt_is_transient_until_consumed() {
    let temp = tempfile::tempdir().unwrap();
    let (agent, entered, release) = controlled(vec![Ok("first".into()), Ok("second".into())], true);
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
        if matches!(live.recv().await.unwrap(), AgentEvent::PromptQueued(text) if text == "queued while busy")
        {
            break;
        }
    }
    assert!(
        !handle.snapshot().iter().any(
            |event| matches!(event, AgentEvent::UserPrompt(text) if text == "queued while busy")
        )
    );

    release.notify_one();
    let mut status = handle.status();
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert_eq!(
        handle
            .snapshot()
            .iter()
            .filter(
                |event| matches!(event, AgentEvent::UserPrompt(text) if text == "queued while busy")
            )
            .count(),
        1
    );
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "prompt-projection")
        .await
        .unwrap();
    assert!(loaded.entries.iter().any(|entry| matches!(
        entry,
        SessionEntry::Message { message: Message::User { content, .. } }
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
                    SessionEntry::Message { message: Message::User { content, .. } }
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
            .filter(
                |event| matches!(event, AgentEvent::Notice(text) if text == "compacted: summary")
            )
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
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
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
async fn mid_turn_auto_compact_before_tool_result_pairs_real_result() {
    // End-to-end race (d): the assistant tool_call is committed, then the
    // in-turn auto-compact fires BEFORE the tool executes. At that point
    // history holds the assistant without its result, so prepare_compaction
    // (via context() -> repair_tool_pairs) synthesizes an interrupted
    // placeholder, which is captured verbatim in the Compaction entry's
    // `retained` snapshot. Only afterwards does the real tool result get
    // committed — i.e. AFTER the Compaction entry. The final derived
    // context must skip the placeholder and pair the real result with its
    // tool_call: no orphan, no unpaired call (the 400-class malformation).
    let temp = tempfile::tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::new(
        Box::new(ScriptedContextCaptureModel {
            replies: vec![
                (
                    AssistantMessage {
                        content: None,
                        tool_calls: vec![ToolCall {
                            id: "call-1".into(),
                            name: "keep_alive".into(),
                            arguments: "{}".into(),
                        }],
                        reasoning: None,
                    },
                    // 80% of the 1000-token window: triggers the in-turn
                    // auto-compact right after the assistant is committed.
                    Some(Usage {
                        input_tokens: 800,
                        output_tokens: 10,
                    }),
                ),
                (
                    AssistantMessage {
                        content: Some("summary".into()),
                        tool_calls: vec![],
                        reasoning: None,
                    },
                    Some(Usage {
                        input_tokens: 900,
                        output_tokens: 20,
                    }),
                ),
                (
                    AssistantMessage {
                        content: Some("done".into()),
                        tool_calls: vec![],
                        reasoning: None,
                    },
                    Some(Usage {
                        input_tokens: 100,
                        output_tokens: 5,
                    }),
                ),
            ]
            .into(),
            calls: calls.clone(),
        }),
        vec![Box::new(KeepAliveTool { sender: None })],
    );
    agent.set_context_window(1000);
    // Prior conversation so there is something before the current turn to
    // compact (prepare_compaction requires an assistant/tool message before
    // the retained user turn).
    agent.restore_history(vec![
        Message::User {
            content: "old question".into(),
            images: vec![],
        }
        .into(),
        Message::Assistant(AssistantMessage {
            content: Some("old answer".into()),
            tool_calls: vec![],
            reasoning: None,
        })
        .into(),
    ]);
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "auto-compact-race".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let task = runner.start(Some("current question".into()));
    let mut status = handle.status();
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert_eq!(
        result,
        SessionStatus::Finished(SessionResult::Completed(Some("done".into())))
    );
    task.join().await.unwrap();

    // Model calls: [0] first round, [1] compaction summary, [2] round after
    // the real result landed. The last context must be well-formed. The
    // guard is scoped so it is dropped before the awaited reload below.
    {
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            3,
            "expected round, compaction, round: {calls:?}"
        );
        let final_context = &calls[2];
        assert!(matches!(
            &final_context[0],
            Message::User { content, .. }
                if content == "[compacted summary of earlier conversation]\nsummary"
        ));
        // Placeholder skipped entirely: the only Tool message is the real
        // result, paired with its tool_call.
        assert!(
            !final_context
                .iter()
                .any(|m| matches!(m, Message::Tool { content, .. }
                if content == "[turn interrupted before a tool result was produced]"))
        );
        assert_eq!(
            final_context
                .iter()
                .filter(|m| matches!(m, Message::Tool { .. }))
                .count(),
            1
        );
        assert!(matches!(
            final_context.last().unwrap(),
            Message::Tool {
                call_id,
                name,
                content,
                is_error: false,
                synthetic: false,
                ..
            } if call_id == "call-1" && name == "keep_alive" && content.is_empty()
        ));
        // No orphan and no unpaired call: repairing the derived context is
        // a fixed point (a 400-class malformation would repair into
        // something different, dropping the orphan or flushing a
        // placeholder).
        assert_eq!(repair_tool_pairs(final_context.clone()), *final_context);
    }

    // The persisted log pins the race order: the Compaction entry's
    // retained snapshot holds the synthetic placeholder, and the real
    // result entry comes AFTER the Compaction entry.
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "auto-compact-race")
        .await
        .unwrap();
    let compaction_index = loaded
        .entries
        .iter()
        .position(|entry| matches!(entry, SessionEntry::Compaction { .. }))
        .expect("auto-compact must persist a Compaction entry");
    assert!(matches!(
        &loaded.entries[compaction_index],
        SessionEntry::Compaction { retained, .. }
            if retained.iter().any(|m| matches!(
                m,
                Message::Tool { content, is_error: true, synthetic: true, .. }
                    if content == "[turn interrupted before a tool result was produced]"
            ))
    ));
    assert!(
        loaded.entries[compaction_index + 1..]
            .iter()
            .any(|entry| matches!(
                entry,
                SessionEntry::Message {
                    message: Message::Tool {
                        call_id,
                        name,
                        content,
                        is_error: false,
                        synthetic: false,
                        ..
                    }
                } if call_id == "call-1" && name == "keep_alive" && content.is_empty()
            ))
    );
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
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
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
    assert!(
        !handle.snapshot().iter().any(
            |event| matches!(event, AgentEvent::Notice(text) if text.starts_with("compacted:"))
        )
    );
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
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
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
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
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
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert_eq!(result, SessionStatus::Finished(SessionResult::Cancelled));
    assert!(
        !handle.snapshot().iter().any(
            |event| matches!(event, AgentEvent::Notice(text) if text.starts_with("compacted:"))
        )
    );
    task.join().await.unwrap();
}

#[tokio::test]
async fn runner_strips_image_marker_and_attaches_synthetic_user() {
    struct MarkerTool;
    #[async_trait]
    impl Tool for MarkerTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "read_image".into(),
                description: "test".into(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }
        async fn execute(&self, _: Value) -> Result<String, String> {
            Ok("__EA_IMAGE__hash123,image/png__EA_IMAGE_END__[image read: pic.png] (hash hash123, image/png, 3 bytes)".into())
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::new(
        Box::new(ScriptedContextCaptureModel {
            replies: vec![
                (
                    AssistantMessage {
                        content: None,
                        tool_calls: vec![ToolCall {
                            id: "call-img".into(),
                            name: "read_image".into(),
                            arguments: r#"{"path":"pic.png"}"#.into(),
                        }],
                        reasoning: None,
                    },
                    None,
                ),
                (
                    AssistantMessage {
                        content: Some("done".into()),
                        tool_calls: vec![],
                        reasoning: None,
                    },
                    None,
                ),
            ]
            .into(),
            calls: calls.clone(),
        }),
        vec![Box::new(MarkerTool)],
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "marker".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let task = runner.start(Some("look".into()));
    let mut status = handle.status();
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert_eq!(
        result,
        SessionStatus::Finished(SessionResult::Completed(Some("done".into())))
    );
    task.join().await.unwrap();

    // Persisted tool entry has the summary only; the synthetic user message
    // carrying the image reference follows it.
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "marker")
        .await
        .unwrap();
    let tool = loaded
        .entries
        .iter()
        .find_map(|entry| match entry {
            SessionEntry::Message {
                message: Message::Tool { content, .. },
            } => Some(content.clone()),
            _ => None,
        })
        .unwrap();
    assert!(tool.starts_with("[image read: pic.png]"));
    assert!(!tool.contains("__EA_IMAGE__"));
    let synthetic = loaded
        .entries
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::Message {
                message: Message::User { content, images },
            } => Some((content.clone(), images.clone())),
            _ => None,
        })
        .find(|(content, _)| content.starts_with("[image attached:"));
    let (content, images) = synthetic.expect("synthetic image user message");
    assert_eq!(content, "[image attached: pic.png]");
    assert_eq!(
        images,
        vec![crate::agent::ImagePart {
            hash: "hash123".into(),
            mime: "image/png".into(),
        }]
    );
    // The second model call saw the image on the user message.
    let calls = calls.lock().unwrap();
    assert!(calls[1].iter().any(|message| matches!(
        message,
        Message::User { images, .. } if !images.is_empty()
    )));
}

#[tokio::test]
async fn prompt_with_image_rides_on_the_committed_user_message() {
    let temp = tempfile::tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::new(
        Box::new(ScriptedContextCaptureModel {
            replies: vec![(
                AssistantMessage {
                    content: Some("answered".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
                None,
            )]
            .into(),
            calls: calls.clone(),
        }),
        vec![],
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "attach".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let task = runner.start(None);
    handle.prompt_with_image(
        "what is this?",
        crate::agent::ImagePart {
            hash: "abc".into(),
            mime: "image/jpeg".into(),
        },
    );
    let mut status = handle.status();
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert_eq!(
        result,
        SessionStatus::Finished(SessionResult::Completed(Some("answered".into())))
    );
    task.join().await.unwrap();
    let calls = calls.lock().unwrap();
    let user = calls[0]
        .iter()
        .find_map(|message| match message {
            Message::User { content, images } => Some((content.clone(), images.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(user.0, "what is this?");
    assert_eq!(
        user.1,
        vec![crate::agent::ImagePart {
            hash: "abc".into(),
            mime: "image/jpeg".into(),
        }]
    );
}
