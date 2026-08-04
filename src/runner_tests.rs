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

/// `ScriptedContextCaptureModel` that reports vision support, so tests can
/// exercise the image-attachment paths that require a vision-capable model
/// (the runner skips read_image attachments and rejects explicit `/image`
/// prompts on non-vision models).
struct VisionScriptedContextCaptureModel(ScriptedContextCaptureModel);

#[async_trait]
impl Model for VisionScriptedContextCaptureModel {
    async fn complete(
        &mut self,
        messages: &[Message],
        tools: &[ToolSpec],
        on_delta: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        self.0.complete(messages, tools, on_delta).await
    }

    fn supports_vision(&self) -> bool {
        true
    }
}

/// Emits a read_image result carrying the structured image marker; shared by
/// the runner tests covering marker splitting and synthetic attachment.
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

/// Scripted model with a `vision` flag and an optional simulation of the
/// wire vision gate: when `error_on_images` is set, any request carrying a
/// `Message::User` image fails — exactly like `ensure_vision_supported` on
/// the real wires. Used to prove a poisoned history would surface as a
/// failed model call, so tests can assert the session was NOT poisoned.
struct GateScriptedModel {
    replies: VecDeque<(AssistantMessage, Option<Usage>)>,
    calls: Arc<Mutex<Vec<Vec<Message>>>>,
    vision: bool,
    error_on_images: bool,
}

#[async_trait]
impl Model for GateScriptedModel {
    fn supports_vision(&self) -> bool {
        self.vision
    }

    async fn complete(
        &mut self,
        messages: &[Message],
        _: &[ToolSpec],
        _: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        self.calls.lock().unwrap().push(messages.to_vec());
        if self.error_on_images
            && messages
                .iter()
                .any(|m| matches!(m, Message::User { images, .. } if !images.is_empty()))
        {
            anyhow::bail!("model does not support image input");
        }
        Ok(self.replies.pop_front().expect("unexpected model call"))
    }
}

/// Marker-emitting read_image that blocks until released, so a test can
/// queue a `SwitchModel` command while the tool is still executing (the
/// command lands in `wait_for_operation`'s pending cache).
struct BlockingMarkerTool {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl Tool for BlockingMarkerTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_image".into(),
            description: "test".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }
    async fn execute(&self, _: Value) -> Result<String, String> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok("__EA_IMAGE__hash123,image/png__EA_IMAGE_END__[image read: pic.png] (hash hash123, image/png, 3 bytes)".into())
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
        vec![
            Box::new(CompletingWithCancelTool {
                commands: commands.clone(),
            }),
            // Keeps the background-completion channel open so the session
            // can stay Idle after the cancel (see `recovering_agent`).
            Box::new(KeepAliveTool { sender: None }),
        ],
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "tool-cancel".into(),
        IdlePolicy::FinishWhenIdle,
    );
    *commands.lock().unwrap() = Some(handle.commands.clone());
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(Some("prompt".into()));
    // The tool result is committed before the racing Cancel is processed, and
    // the Cancel only cancels the turn: the FinishWhenIdle session returns to
    // Idle and stays alive instead of finalizing (composer cancel must not
    // kill a delegate subagent). The "turn cancelled" notice is emitted only
    // after the tool entry commit, so it pins the commit-before-cancel order.
    loop {
        if matches!(
            live.recv().await.unwrap(),
            AgentEvent::Notice(text) if text == "turn cancelled"
        ) {
            break;
        }
    }
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;
    assert!(!matches!(*status.borrow(), SessionStatus::Finished(_)));
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
    drop(handle);
    drop(task);
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
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(None);
    entered.notified().await;
    handle.cancel();
    // The in-flight compaction is cancelled but the Cancel command only
    // cancels the turn: the FinishWhenIdle session returns to Idle and stays
    // alive instead of finalizing. The "compaction cancelled" notice is
    // emitted only after the operation is dropped, pinning the order.
    loop {
        if matches!(
            live.recv().await.unwrap(),
            AgentEvent::Notice(text) if text == "compaction cancelled"
        ) {
            break;
        }
    }
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;
    assert!(!matches!(*status.borrow(), SessionStatus::Finished(_)));
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
    drop(handle);
    drop(task);
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
        // Keeps the background-completion channel open so the session can
        // stay Idle after the cancel (see `recovering_agent`).
        vec![Box::new(KeepAliveTool { sender: None })],
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "round-cancel".into(),
        IdlePolicy::FinishWhenIdle,
    );
    *commands.lock().unwrap() = Some(handle.commands.clone());
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(Some("prompt".into()));
    // The completed round is committed before the racing Cancel is processed;
    // the Cancel only cancels the turn, so the FinishWhenIdle session returns
    // to Idle and stays alive (composer cancel must not kill a delegate). The
    // "turn cancelled" notice is emitted only after the round commit.
    loop {
        if matches!(
            live.recv().await.unwrap(),
            AgentEvent::Notice(text) if text == "turn cancelled"
        ) {
            break;
        }
    }
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;
    assert!(!matches!(*status.borrow(), SessionStatus::Finished(_)));
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "round-cancel")
        .await
        .unwrap();
    assert!(loaded.entries.iter().any(|entry| matches!(
        entry,
        SessionEntry::Message { message: Message::Assistant(message) }
            if message.content.as_deref() == Some("completed answer")
    )));
    drop(handle);
    drop(task);
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
        // Keeps the background-completion channel open so the session can
        // stay Idle after the cancel (see `recovering_agent`).
        vec![Box::new(KeepAliveTool { sender: None })],
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
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(None);
    // The completed compaction is committed before the racing Cancel is
    // processed; the Cancel only cancels the turn, so the FinishWhenIdle
    // session returns to Idle and stays alive instead of finalizing. The
    // projection notice pins the commit-before-cancel order.
    loop {
        if matches!(
            live.recv().await.unwrap(),
            AgentEvent::Notice(text) if text == "compacted: completed summary"
        ) {
            break;
        }
    }
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;
    assert!(!matches!(*status.borrow(), SessionStatus::Finished(_)));
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
    drop(handle);
    drop(task);
}

#[tokio::test]
async fn cancel_keeps_finish_when_idle_session_alive_for_follow_up() {
    // The composer cancel on a delegate subagent (FinishWhenIdle) must only
    // cancel the current turn: the session returns to Idle, does NOT
    // finalize, and still accepts a follow-up message. Natural completion of
    // that follow-up turn still finalizes.
    let temp = tempfile::tempdir().unwrap();
    let (agent, entered, _release) = recovering_agent(
        vec![
            // Consumed by the blocked first call, never delivered.
            Ok(AssistantMessage {
                content: Some("interrupted".into()),
                tool_calls: Vec::new(),
                reasoning: None,
            }),
            Ok(AssistantMessage {
                content: Some("follow-up answer".into()),
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
        "cancel-alive".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(Some("prompt".into()));
    entered.notified().await;
    handle.cancel();

    // Cancelled turn -> Idle, not Finished: the delegate stays alive.
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;
    assert!(
        !matches!(*status.borrow(), SessionStatus::Finished(_)),
        "cancel must not finalize a FinishWhenIdle session"
    );

    // A follow-up message opens a new turn that completes naturally; with no
    // queued work left, the session then finalizes as Completed.
    handle.prompt("follow up");
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "follow-up answer" => break,
            AgentEvent::Error(_) => panic!("follow-up turn failed"),
            _ => {}
        }
    }
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert_eq!(
        result,
        SessionStatus::Finished(SessionResult::Completed(Some("follow-up answer".into())))
    );
    task.join().await.unwrap();
}

#[tokio::test]
async fn cancel_then_compact_keeps_finish_when_idle_session_alive_for_follow_up() {
    // A Cancel interruption must survive maintenance work: queuing a Compact
    // after the cancel must NOT clear the cancel flag (`has_work()` includes
    // `PendingCommand::Compact`, not just prompts), otherwise the compacted
    // FinishWhenIdle session would finalize at the next idle point. The flag
    // is cleared only when a real follow-up prompt opens a new turn, so the
    // session stays Idle after the compaction and the follow-up message
    // completes naturally into Finished(Completed).
    let temp = tempfile::tempdir().unwrap();
    let (mut agent, entered, _release) = recovering_agent(
        vec![
            // Consumed by the blocked first call, never delivered.
            Ok(AssistantMessage {
                content: Some("interrupted".into()),
                tool_calls: Vec::new(),
                reasoning: None,
            }),
            // Compaction call.
            Ok(AssistantMessage {
                content: Some("compact summary".into()),
                tool_calls: Vec::new(),
                reasoning: None,
            }),
            // Follow-up turn.
            Ok(AssistantMessage {
                content: Some("follow-up answer".into()),
                tool_calls: Vec::new(),
                reasoning: None,
            }),
        ],
        true,
    );
    history_for_compaction(&mut agent);
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "cancel-compact-alive".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(Some("prompt".into()));
    entered.notified().await;
    handle.cancel();

    // Cancelled turn -> Idle, not Finished: the delegate stays alive.
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;
    assert!(!matches!(*status.borrow(), SessionStatus::Finished(_)));

    // Compact while the cancel flag is still set: the completed compaction
    // must return the session to Idle, never finalize it. If the queued
    // Compact had wrongly cleared the flag, the runner would finalize
    // right after the projection notice — wait for either state and assert
    // it parked at Idle instead.
    handle.compact();
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::Notice(text) if text == "compacted: compact summary" => break,
            AgentEvent::Error(_) => panic!("compaction failed"),
            _ => {}
        }
    }
    let after_compact = wait_for_status(&mut status, |s| {
        matches!(s, SessionStatus::Idle | SessionStatus::Finished(_))
    })
    .await;
    assert!(
        matches!(after_compact, SessionStatus::Idle),
        "a Compact queued after Cancel must not finalize the session"
    );

    // The follow-up prompt opens a new turn that completes naturally; with
    // no queued work left, the session then finalizes as Completed.
    handle.prompt("follow up");
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "follow-up answer" => break,
            AgentEvent::Error(_) => panic!("follow-up turn failed"),
            _ => {}
        }
    }
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert_eq!(
        result,
        SessionStatus::Finished(SessionResult::Completed(Some("follow-up answer".into())))
    );
    task.join().await.unwrap();
}

#[tokio::test]
async fn cancel_with_queued_message_keeps_processing_it() {
    // A Cancel command that interrupts a turn must not drop queued messages:
    // the runner re-queues them (wait_for_operation's pending batch) and the
    // next turn processes them normally.
    let temp = tempfile::tempdir().unwrap();
    let (agent, entered, _) = recovering_agent(
        vec![
            // Consumed by the blocked first call, never delivered.
            Ok(AssistantMessage {
                content: Some("interrupted".into()),
                tool_calls: Vec::new(),
                reasoning: None,
            }),
            Ok(AssistantMessage {
                content: Some("queued answer".into()),
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
        "cancel-queued".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(Some("prompt".into()));
    entered.notified().await;
    handle.prompt("queued before cancel");
    handle.cancel();

    // The queued prompt survives the cancel and opens a fresh turn.
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::UserPrompt(text) if text == "queued before cancel" => break,
            AgentEvent::Error(_) => panic!("queued turn failed"),
            _ => {}
        }
    }
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "queued answer" => break,
            AgentEvent::Error(_) => panic!("queued turn failed"),
            _ => {}
        }
    }
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert_eq!(
        result,
        SessionStatus::Finished(SessionResult::Completed(Some("queued answer".into())))
    );
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "cancel-queued")
        .await
        .unwrap();
    assert!(loaded.entries.iter().any(|entry| matches!(
        entry,
        SessionEntry::Message { message: Message::User { content, .. } }
            if content == "queued before cancel"
    )));
    task.join().await.unwrap();
}

#[tokio::test]
async fn natural_completion_of_finish_when_idle_still_finalizes() {
    // The fix only changes the Cancel-command path: a delegate subagent that
    // completes its turn with no queued work and no running background must
    // still finalize (Finished(Completed)) as before.
    let temp = tempfile::tempdir().unwrap();
    let (agent, _, _) = controlled(vec![Ok("done".into())], false);
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "natural-complete".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let task = runner.start(Some("prompt".into()));
    let mut status = handle.status();
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert_eq!(
        result,
        SessionStatus::Finished(SessionResult::Completed(Some("done".into())))
    );
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "natural-complete")
        .await
        .unwrap();
    assert!(loaded.entries.iter().any(|entry| matches!(
        entry,
        SessionEntry::Message { message: Message::Assistant(message) }
            if message.content.as_deref() == Some("done")
    )));
    task.join().await.unwrap();
}

#[tokio::test]
async fn runner_strips_image_marker_and_skips_attachment_without_vision() {
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
        "marker-novision".into(),
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

    // Persisted tool entry has the summary only, annotated that the image
    // attachment was skipped (the vision gate would otherwise lock the
    // session); no synthetic user message carries the image.
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "marker-novision")
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
    assert!(tool.contains("已跳过附加"));
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
    assert!(
        synthetic.is_none(),
        "no synthetic image user message on a non-vision model"
    );
    // The second model call saw no images.
    let calls = calls.lock().unwrap();
    assert!(
        calls[1]
            .iter()
            .all(|message| !matches!(message, Message::User { images, .. } if !images.is_empty()))
    );
}

#[tokio::test]
async fn runner_attaches_image_marker_with_vision_model() {
    let temp = tempfile::tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::new(
        Box::new(VisionScriptedContextCaptureModel(
            ScriptedContextCaptureModel {
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
            },
        )),
        vec![Box::new(MarkerTool)],
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "marker-vision".into(),
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
        .load(temp.path(), "marker-vision")
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
    assert!(!tool.contains("已跳过附加"));
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
async fn switch_to_non_vision_during_read_image_skips_attachment() {
    // Race: the session starts on a vision model, calls read_image, and the
    // user switches to a non-vision model WHILE the tool is executing. The
    // switch must take effect before the marker is interpreted: the image
    // must NOT be attached, and the next (non-vision) model call — which
    // simulates the wire vision gate by erroring on image-bearing requests
    // — must succeed.
    let temp = tempfile::tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let vision = GateScriptedModel {
        replies: vec![(
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
        )]
        .into(),
        calls: calls.clone(),
        vision: true,
        error_on_images: false,
    };
    let non_vision = GateScriptedModel {
        replies: vec![(
            AssistantMessage {
                content: Some("done".into()),
                tool_calls: vec![],
                reasoning: None,
            },
            None,
        )]
        .into(),
        calls: calls.clone(),
        vision: false,
        error_on_images: true,
    };
    let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let agent = Agent::new(
        Box::new(vision),
        vec![Box::new(BlockingMarkerTool {
            entered: entered.clone(),
            release: release.clone(),
        })],
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "switch-v2nv".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let task = runner.start(Some("look".into()));
    entered.notified().await;
    handle.switch_model(Box::new(non_vision));
    release.notify_one();
    let mut status = handle.status();
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert_eq!(
        result,
        SessionStatus::Finished(SessionResult::Completed(Some("done".into())))
    );
    task.join().await.unwrap();

    let loaded = SessionStore::Jsonl
        .load(temp.path(), "switch-v2nv")
        .await
        .unwrap();
    // No synthetic image user message was committed under the new model.
    let synthetic = loaded.entries.iter().any(|entry| match entry {
        SessionEntry::Message {
            message: Message::User { content, images },
        } => content.starts_with("[image attached:") && !images.is_empty(),
        _ => false,
    });
    assert!(
        !synthetic,
        "image must not be attached after switch to non-vision"
    );
    // The tool summary carries the skip annotation.
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
    assert!(tool.contains("已跳过附加"));
    // The non-vision model's request was clean (it would have errored on an
    // image-bearing request — the session was not poisoned).
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls[1].iter().all(|message| !matches!(
        message,
        Message::User { images, .. } if !images.is_empty()
    )));
}

#[tokio::test]
async fn switch_to_vision_during_read_image_attaches() {
    // Inverse race: session starts on a non-vision model, calls read_image,
    // and the user switches to a vision model WHILE the tool is executing.
    // The image must be attached (the guard must see the NEW model) and
    // reach the next model call.
    let temp = tempfile::tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let non_vision = GateScriptedModel {
        replies: vec![(
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
        )]
        .into(),
        calls: calls.clone(),
        vision: false,
        error_on_images: false,
    };
    let vision = GateScriptedModel {
        replies: vec![(
            AssistantMessage {
                content: Some("done".into()),
                tool_calls: vec![],
                reasoning: None,
            },
            None,
        )]
        .into(),
        calls: calls.clone(),
        vision: true,
        error_on_images: false,
    };
    let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let agent = Agent::new(
        Box::new(non_vision),
        vec![Box::new(BlockingMarkerTool {
            entered: entered.clone(),
            release: release.clone(),
        })],
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "switch-nv2v".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let task = runner.start(Some("look".into()));
    entered.notified().await;
    handle.switch_model(Box::new(vision));
    release.notify_one();
    let mut status = handle.status();
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert_eq!(
        result,
        SessionStatus::Finished(SessionResult::Completed(Some("done".into())))
    );
    task.join().await.unwrap();

    let loaded = SessionStore::Jsonl
        .load(temp.path(), "switch-nv2v")
        .await
        .unwrap();
    // The synthetic image user message was committed and carries the image.
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
    // The vision model's next request saw the image.
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
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
        Box::new(VisionScriptedContextCaptureModel(
            ScriptedContextCaptureModel {
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
            },
        )),
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

#[tokio::test]
async fn prompt_with_image_rejected_on_non_vision_model() {
    // Explicit `/image` entrance on a non-vision model is rejected loudly:
    // an Error event is surfaced, nothing is committed (no poisoned User
    // message), and the session survives instead of terminating as Failed.
    let temp = tempfile::tempdir().unwrap();
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
            calls: Arc::new(Mutex::new(Vec::new())),
        }),
        vec![],
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "reject-image".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(None);
    handle.prompt_with_image(
        "what is this?",
        crate::agent::ImagePart {
            hash: "abc".into(),
            mime: "image/jpeg".into(),
        },
    );
    loop {
        if let AgentEvent::Error(text) = live.recv().await.unwrap() {
            assert!(text.contains("不支持图片"));
            break;
        }
    }
    // The session was not killed by the rejection: it returns to Idle and
    // finalizes normally (Completed, not Failed).
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert_eq!(
        result,
        SessionStatus::Finished(SessionResult::Completed(None))
    );
    task.join().await.unwrap();
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "reject-image")
        .await
        .unwrap();
    assert!(
        loaded.entries.iter().all(|entry| !matches!(
            entry,
            SessionEntry::Message {
                message: Message::User { content, images }
            } if content == "what is this?" || !images.is_empty()
        )),
        "rejected prompt with image must not be persisted"
    );
}

#[tokio::test]
async fn prompt_with_image_rejected_then_plain_prompt_succeeds() {
    // WaitForInput variant: after the `/image` rejection the session returns
    // to Idle and keeps accepting input — a follow-up plain prompt is
    // answered normally, and the rejected image never lands in history.
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
        // KeepAliveTool holds a background-channel sender, so the
        // WaitForInput session survives the idle select after the rejection
        // (with no sender-storing tool the session would terminate Closed
        // at idle — the runner treats a closed background channel as
        // "no more background work").
        vec![Box::new(KeepAliveTool { sender: None })],
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "reject-then-plain".into(),
        IdlePolicy::WaitForInput,
    );
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(None);
    handle.prompt_with_image(
        "what is this?",
        crate::agent::ImagePart {
            hash: "abc".into(),
            mime: "image/jpeg".into(),
        },
    );
    loop {
        let ev = live.recv().await.unwrap();
        if let AgentEvent::Error(text) = ev {
            assert!(text.contains("不支持图片"));
            break;
        }
    }
    // Back to Idle after the rejection.
    let idle = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;
    assert_eq!(idle, SessionStatus::Idle);
    // A plain follow-up prompt is answered normally.
    handle.prompt("plain question");
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "answered" => break,
            AgentEvent::Error(text) => panic!("follow-up turn failed: {text}"),
            _ => {}
        }
    }
    drop(handle);
    task.join().await.unwrap();
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "reject-then-plain")
        .await
        .unwrap();
    // The plain prompt was persisted; no image-bearing message ever was.
    assert!(loaded.entries.iter().any(|entry| matches!(
        entry,
        SessionEntry::Message {
            message: Message::User { content, .. }
        } if content == "plain question"
    )));
    assert!(
        loaded.entries.iter().all(|entry| !matches!(
            entry,
            SessionEntry::Message {
                message: Message::User { images, .. }
            } if !images.is_empty()
        )),
        "rejected image must never be persisted"
    );
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Background bash + FinishWhenIdle (detached vs blocking)
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

/// Mock bash tool: any background call returns the "started background
/// task N: label" message (so `Agent::after_tool_entry` tracks the id) and
/// captures the agent's completion sender so a test can deliver
/// `BackgroundCompleted` later at its own pace.
struct MockBackgroundBash {
    id: u64,
    label: &'static str,
    sender: Arc<Mutex<Option<mpsc::UnboundedSender<AgentEvent>>>>,
}

#[async_trait]
impl Tool for MockBackgroundBash {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "test only".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }
    async fn execute(&self, _: Value) -> Result<String, String> {
        Ok(format!(
            "started background task {}: {}",
            self.id, self.label
        ))
    }
    fn set_event_sender(&mut self, sender: mpsc::UnboundedSender<AgentEvent>) {
        *self.sender.lock().unwrap() = Some(sender);
    }
}

/// Scripted model that records every request context and can block one
/// designated call (1-based) until released — used to deliver a background
/// completion while the last model round is still in flight.
struct BlockingContextCaptureModel {
    replies: VecDeque<AssistantMessage>,
    block_call: usize,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    calls: Arc<Mutex<Vec<Vec<Message>>>>,
    call_count: usize,
}

#[async_trait]
impl Model for BlockingContextCaptureModel {
    async fn complete(
        &mut self,
        messages: &[Message],
        _: &[ToolSpec],
        _: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        self.call_count += 1;
        self.calls.lock().unwrap().push(messages.to_vec());
        if self.call_count == self.block_call {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok((
            self.replies.pop_front().expect("unexpected model call"),
            None,
        ))
    }
}

fn background_bash_call(command: &str, detached: bool) -> ToolCall {
    let mut arguments = serde_json::json!({"command": command, "background": true});
    if detached {
        arguments["detached"] = serde_json::json!(true);
    }
    ToolCall {
        id: "call-1".into(),
        name: "bash".into(),
        arguments: arguments.to_string(),
    }
}

#[tokio::test]
async fn unfinished_background_bash_blocks_finish_when_idle_until_completion() {
    // A non-detached background task keeps the FinishWhenIdle session from
    // finalizing: the turn ends with an intermediate no-tool-call text, the
    // runner parks at Idle (not Finished), and only after the completion is
    // delivered does the model get a new round and the session finalize.
    let temp = tempfile::tempdir().unwrap();
    let sender = Arc::new(Mutex::new(None));
    let agent = Agent::new(
        Box::new(ScriptedAssistantModel {
            replies: vec![
                AssistantMessage {
                    content: None,
                    tool_calls: vec![background_bash_call("cargo build", false)],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("build started, waiting".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("build done, final".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
            ]
            .into(),
        }),
        vec![Box::new(MockBackgroundBash {
            id: 9,
            label: "cargo build",
            sender: sender.clone(),
        })],
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "bg-blocks".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(Some("run build".into()));

    // First turn: background tool call + intermediate text. The session
    // parks at Idle — the unfinished task must block finalization.
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "build started, waiting" => break,
            AgentEvent::Error(text) => panic!("turn failed: {text}"),
            _ => {}
        }
    }
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;
    assert!(
        !matches!(*status.borrow(), SessionStatus::Finished(_)),
        "an unfinished background task must block FinishWhenIdle"
    );

    // Deliver the completion: the runner commits it durably, starts a new
    // model round, and only then finalizes with that round's answer.
    sender
        .lock()
        .unwrap()
        .as_ref()
        .expect("agent must wire the bash completion sender")
        .send(AgentEvent::BackgroundCompleted {
            id: 9,
            output: "build ok".into(),
            label: Some("cargo build".into()),
        })
        .unwrap();
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "build done, final" => break,
            AgentEvent::Error(text) => panic!("follow-up turn failed: {text}"),
            _ => {}
        }
    }
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert_eq!(
        result,
        SessionStatus::Finished(SessionResult::Completed(Some("build done, final".into())))
    );
    task.join().await.unwrap();

    // The completion entry was persisted before the model's follow-up round.
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "bg-blocks")
        .await
        .unwrap();
    assert!(loaded.entries.iter().any(|entry| matches!(
        entry,
        SessionEntry::BackgroundCompletion { id, output, .. } if *id == 9 && output == "build ok"
    )));
}

#[tokio::test]
async fn detached_daemon_does_not_block_finish_when_idle() {
    // A detached daemon (background:true, detached:true) runs in the shared
    // registry but is NOT tracked as a blocking task: the session finalizes
    // promptly after the final no-tool-call text, and the daemon stays alive
    // in the registry until explicitly cancelled.
    let temp = tempfile::tempdir().unwrap();
    let workspace = crate::workspace::Workspace::new(temp.path()).unwrap();
    let (tools, background) = crate::tools::builtins(workspace, None, false, None);
    let agent = Agent::new(
        Box::new(ScriptedAssistantModel {
            replies: vec![
                AssistantMessage {
                    content: None,
                    tool_calls: vec![background_bash_call("sleep 3600", true)],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("daemon started, report done".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
            ]
            .into(),
        }),
        tools,
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "detached-daemon".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(Some("start daemon".into()));

    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "daemon started, report done" => break,
            AgentEvent::Error(text) => panic!("turn failed: {text}"),
            _ => {}
        }
    }
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))),
    )
    .await
    .expect("FinishWhenIdle must finalize with a detached daemon running");
    assert_eq!(
        result,
        SessionStatus::Finished(SessionResult::Completed(Some(
            "daemon started, report done".into()
        )))
    );
    task.join().await.unwrap();

    // The daemon task is still alive in the shared registry.
    let daemons = background.running();
    assert_eq!(daemons.len(), 1, "daemon must stay registered: {daemons:?}");
    assert_eq!(daemons[0].kind, "bash");
    // Explicitly cancel to avoid leaking the sleep process past the test.
    background.cancel(daemons[0].id);
    assert!(background.running().is_empty());
}

#[tokio::test]
async fn content_and_tool_calls_in_same_round_still_block_and_complete() {
    // An assistant message carrying BOTH content and a background tool call
    // must not finalize: the content and the tool result are persisted, the
    // model is called again, and the Completed answer comes from the last
    // no-tool-call round (after the completion was delivered).
    let temp = tempfile::tempdir().unwrap();
    let sender = Arc::new(Mutex::new(None));
    let agent = Agent::new(
        Box::new(ScriptedAssistantModel {
            replies: vec![
                AssistantMessage {
                    content: Some("plan: start build".into()),
                    tool_calls: vec![background_bash_call("cargo build", false)],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("still waiting".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("final done".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
            ]
            .into(),
        }),
        vec![Box::new(MockBackgroundBash {
            id: 9,
            label: "cargo build",
            sender: sender.clone(),
        })],
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "content-calls".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(Some("go".into()));

    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "still waiting" => break,
            AgentEvent::Error(text) => panic!("turn failed: {text}"),
            _ => {}
        }
    }
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;
    assert!(
        !matches!(*status.borrow(), SessionStatus::Finished(_)),
        "content + background tool call must not finalize the session"
    );

    sender
        .lock()
        .unwrap()
        .as_ref()
        .expect("agent must wire the bash completion sender")
        .send(AgentEvent::BackgroundCompleted {
            id: 9,
            output: "build ok".into(),
            label: Some("cargo build".into()),
        })
        .unwrap();
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "final done" => break,
            AgentEvent::Error(text) => panic!("follow-up turn failed: {text}"),
            _ => {}
        }
    }
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert_eq!(
        result,
        SessionStatus::Finished(SessionResult::Completed(Some("final done".into())))
    );
    task.join().await.unwrap();

    // Both the content-bearing assistant message and the tool result were
    // persisted; the answer comes from the last no-tool-call round.
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "content-calls")
        .await
        .unwrap();
    assert!(loaded.entries.iter().any(|entry| matches!(
        entry,
        SessionEntry::Message {
            message: Message::Assistant(message)
        } if message.content.as_deref() == Some("plan: start build")
    )));
    assert!(loaded.entries.iter().any(|entry| matches!(
        entry,
        SessionEntry::Message {
            message: Message::Tool { content, .. }
        } if content == "started background task 9: cargo build"
    )));
}

#[tokio::test]
async fn completion_arriving_during_last_model_round_is_committed_before_finalize() {
    // The completion arrives while the last model request (which produces
    // the final no-tool-call text) is still in flight. The runner must
    // commit the completion and start a model round that responds to it —
    // never finalize with the pre-completion answer.
    let temp = tempfile::tempdir().unwrap();
    let sender = Arc::new(Mutex::new(None));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::new(
        Box::new(BlockingContextCaptureModel {
            replies: vec![
                AssistantMessage {
                    content: None,
                    tool_calls: vec![background_bash_call("cargo build", false)],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("final answer".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("reacted to completion".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
            ]
            .into(),
            block_call: 2,
            entered: entered.clone(),
            release: release.clone(),
            calls: calls.clone(),
            call_count: 0,
        }),
        vec![Box::new(MockBackgroundBash {
            id: 9,
            label: "cargo build",
            sender: sender.clone(),
        })],
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "completion-inflight".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(Some("build".into()));

    // Wait until the final model round is in flight, deliver the completion,
    // then release the round.
    entered.notified().await;
    sender
        .lock()
        .unwrap()
        .as_ref()
        .expect("agent must wire the bash completion sender")
        .send(AgentEvent::BackgroundCompleted {
            id: 9,
            output: "build ok".into(),
            label: Some("cargo build".into()),
        })
        .unwrap();
    release.notify_one();

    // The runner commits the completion first, then starts the responding
    // model round: the session finalizes with THAT round's answer, proving
    // it did not finalize with the pre-completion "final answer".
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "reacted to completion" => break,
            AgentEvent::Error(text) => panic!("turn failed: {text}"),
            _ => {}
        }
    }
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert_eq!(
        result,
        SessionStatus::Finished(SessionResult::Completed(Some(
            "reacted to completion".into()
        )))
    );
    task.join().await.unwrap();

    // The completion entry was durably committed before the responding
    // model round ran (the round's context carries it as a user message).
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "completion-inflight")
        .await
        .unwrap();
    assert!(loaded.entries.iter().any(|entry| matches!(
        entry,
        SessionEntry::BackgroundCompletion { id, output, .. } if *id == 9 && output == "build ok"
    )));
    {
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 3, "round + completion round: {calls:?}");
        assert!(
            calls[2].iter().any(|message| matches!(
                message,
                Message::User { content, .. }
                    if content.contains("[background task 9 completed: cargo build]")
                        && content.contains("build ok")
            )),
            "the responding model round must see the committed completion: {:?}",
            calls[2]
        );
    }
}

#[tokio::test]
async fn empty_content_without_tool_calls_is_a_natural_turn_end() {
    // An assistant message with empty content and no tool calls is a
    // natural turn end: FinishWhenIdle finalizes as Completed(None) —
    // "pure text" must not be implemented as "non-empty content required".
    let temp = tempfile::tempdir().unwrap();
    let agent = Agent::new(
        Box::new(ScriptedAssistantModel {
            replies: vec![AssistantMessage {
                content: None,
                tool_calls: vec![],
                reasoning: None,
            }]
            .into(),
        }),
        vec![Box::new(KeepAliveTool { sender: None })],
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "empty-content".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let task = runner.start(Some("prompt".into()));
    let mut status = handle.status();
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert_eq!(
        result,
        SessionStatus::Finished(SessionResult::Completed(None))
    );
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "empty-content")
        .await
        .unwrap();
    assert!(loaded.entries.iter().any(|entry| matches!(
        entry,
        SessionEntry::Message {
            message: Message::Assistant(message)
        } if message.tool_calls.is_empty()
    )));
    task.join().await.unwrap();
}
