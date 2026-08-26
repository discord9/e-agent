use super::*;
use crate::agent::{
    AssistantMessage, Model, ModelDeltaKind, POLL_GUARD_ERROR, Tool, ToolOutput, Usage,
    repair_tool_pairs,
};
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

#[test]
fn error_entry_is_excluded_from_context_but_projected_to_error_event() {
    // The dual contract: `SessionEntry::Error` stays out of the provider
    // context (Agent::context), but replays as `AgentEvent::Error` so a
    // resumed or late-attached view can audit it.
    let mut agent = Agent::new(
        Box::new(ControlledModel {
            replies: VecDeque::new(),
            block_first: false,
            entered: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        }),
        vec![],
    );
    agent.restore_history(vec![
        Message::User {
            content: "hello".into(),
            images: vec![],
        }
        .into(),
        SessionEntry::Error {
            text: "provider exploded".into(),
        },
    ]);
    let context = agent.context();
    assert_eq!(context.len(), 1, "only the user message reaches context");
    assert_eq!(
        context[0],
        Message::User {
            content: "hello".into(),
            images: vec![],
        }
    );
    assert!(
        !serde_json::to_string(&context)
            .unwrap()
            .contains("provider exploded"),
        "error text must never reach the provider wire"
    );
    // Projection for replay/late attach.
    assert_eq!(
        entry_event(&SessionEntry::Error {
            text: "provider exploded".into()
        }),
        Some(AgentEvent::Error("provider exploded".into()))
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

/// Scripted model whose first call fails and later calls succeed, capturing
/// every context for inspection — proves a durable Error entry never leaks
/// into a later provider call.
struct FailOnceContextCaptureModel {
    fail_first: bool,
    calls: Arc<Mutex<Vec<Vec<Message>>>>,
    reply: AssistantMessage,
}

#[async_trait]
impl Model for FailOnceContextCaptureModel {
    async fn complete(
        &mut self,
        messages: &[Message],
        _: &[ToolSpec],
        _: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        self.calls.lock().unwrap().push(messages.to_vec());
        if self.fail_first {
            self.fail_first = false;
            anyhow::bail!("provider exploded");
        }
        Ok((self.reply.clone(), None))
    }
}

/// Emits a structured image-bearing tool result (content + image refs);
/// shared by the runner tests covering canonical image-bearing Tool entries
/// and non-vision request stripping.
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
    async fn execute(&self, _: Value) -> Result<ToolOutput, String> {
        Ok(ToolOutput {
            content: "[image read: pic.png] (hash hash123, image/png, 3 bytes)".into(),
            images: vec![crate::agent::ImagePart {
                hash: "hash123".into(),
                mime: "image/png".into(),
            }],
        })
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

/// Image-bearing read_image that blocks until released, so a test can
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
    async fn execute(&self, _: Value) -> Result<ToolOutput, String> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(ToolOutput {
            content: "[image read: pic.png] (hash hash123, image/png, 3 bytes)".into(),
            images: vec![crate::agent::ImagePart {
                hash: "hash123".into(),
                mime: "image/png".into(),
            }],
        })
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
    async fn execute(&self, _: Value) -> Result<ToolOutput, String> {
        self.commands
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .send(SessionCommand::Cancel)
            .unwrap();
        Ok(ToolOutput::text("completed tool result"))
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
    async fn execute(&self, _: Value) -> Result<ToolOutput, String> {
        Ok(ToolOutput::text(String::new()))
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
    let (mut agent, entered, release) = controlled(vec![Ok("The user asked an earlier question and the assistant replied; the conversation then moved on to the latest request, which is still being worked on.".into())], true);
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
        matches!(live.recv().await.unwrap(), AgentEvent::Notice(text) if text == "compacted: The user asked an earlier question and the assistant replied; the conversation then moved on to the latest request, which is still being worked on.")
    );
    let snapshot = handle.snapshot();
    assert_eq!(
        snapshot
            .iter()
            .filter(
                |event| matches!(event, AgentEvent::Notice(text) if text == "compacted: The user asked an earlier question and the assistant replied; the conversation then moved on to the latest request, which is still being worked on.")
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
async fn failed_compaction_persists_error_without_compaction_entry() {
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
    // The failure itself is durable: exactly one Error entry, and it
    // projects back to an `AgentEvent::Error` on reload.
    let errors: Vec<&SessionEntry> = loaded
        .entries
        .iter()
        .filter(|entry| matches!(entry, SessionEntry::Error { .. }))
        .collect();
    assert_eq!(
        errors.len(),
        1,
        "one root error, one Error entry: {loaded:?}"
    );
    assert!(
        matches!(errors[0], SessionEntry::Error { text } if text.contains("compaction error") && text.contains("provider failed"))
    );
    assert_eq!(
        entry_event(errors[0]),
        Some(AgentEvent::Error(
            "compaction error: provider failed".to_owned()
        ))
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
    // The "nothing to compact" failure is a real harness error: durable.
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "empty")
        .await
        .unwrap();
    assert!(
        loaded.entries.iter().any(|entry| matches!(
            entry,
            SessionEntry::Error { text } if text.contains("nothing to compact")
        )),
        "empty-manual-compaction error must be persisted: {loaded:?}"
    );
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
            Ok("The user asked an earlier question and the assistant replied; the conversation then moved on to the latest request, which is still being worked on.".into()),
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
                |event| matches!(event, AgentEvent::Notice(text) if text == "compacted: The user asked an earlier question and the assistant replied; the conversation then moved on to the latest request, which is still being worked on.")
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
async fn regular_turn_persists_usage_row() {
    let temp = tempfile::tempdir().unwrap();
    let sid = format!("runner-usage-{}", crate::session::new_id());
    let store = SessionStore::connect(
        &crate::config::SessionBackend::Sqlite { path: None },
        temp.path(),
        &sid,
    )
    .await
    .expect("connect sqlite store");
    let agent = Agent::new(
        Box::new(ScriptedContextCaptureModel {
            replies: VecDeque::from([(
                AssistantMessage {
                    content: Some("hi".into()),
                    tool_calls: Vec::new(),
                    reasoning: None,
                },
                Some(Usage {
                    input_tokens: 111,
                    output_tokens: 22,
                    ..Default::default()
                }),
            )]),
            calls: Arc::new(Mutex::new(Vec::new())),
        }),
        vec![],
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        store.clone(),
        temp.path().into(),
        sid.clone(),
        IdlePolicy::FinishWhenIdle,
    );
    let task = runner.start(Some("hello".into()));
    let mut status = handle.status();
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert!(matches!(
        result,
        SessionStatus::Finished(SessionResult::Completed(Some(answer))) if answer == "hi"
    ));
    task.join().await.unwrap();

    // 正常轮 usage 已通过 runner 落盘（kind="regular"）。
    let rows = store
        .usage_summary(temp.path())
        .await
        .expect("usage summary");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].session_id, sid);
    assert_eq!(rows[0].kind, "regular");
    // ScriptedContextCaptureModel 未覆写 name() → 默认 "?"。
    assert_eq!(rows[0].model, "?");
    assert_eq!(rows[0].input_tokens, 111);
    assert_eq!(rows[0].output_tokens, 22);
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

    // The failed round committed nothing but the initial user prompt plus
    // the durable harness Error entry (the failure is audit-persisted, the
    // failed assistant output is not).
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "recoverable-failure")
        .await
        .unwrap();
    assert_eq!(loaded.entries.len(), 2);
    assert!(matches!(
        &loaded.entries[0],
        SessionEntry::Message {
            message: Message::User { content, .. }
        } if content == "initial"
    ));
    assert!(
        matches!(
            &loaded.entries[1],
            SessionEntry::Error { text }
                if text.contains("model call failed") && text.contains("model boom")
        ),
        "the failed model call must be durably recorded: {:?}",
        loaded.entries[1]
    );

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
async fn model_call_failure_persists_error_and_keeps_it_out_of_next_context() {
    // Durable-audit + context-exclusion contract: the failed call's Error
    // entry survives a reload, and the error text never appears in the
    // next provider call's context.
    let temp = tempfile::tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Vec::<Vec<Message>>::new()));
    let agent = Agent::new(
        Box::new(FailOnceContextCaptureModel {
            fail_first: true,
            calls: calls.clone(),
            reply: AssistantMessage {
                content: Some("recovered".into()),
                tool_calls: Vec::new(),
                reasoning: None,
            },
        }),
        // KeepAliveTool holds a background-channel sender so the
        // WaitForInput session survives the idle select after the failure.
        vec![Box::new(KeepAliveTool { sender: None })],
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "error-context".into(),
        IdlePolicy::WaitForInput,
    );
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(Some("initial".into()));
    loop {
        if matches!(
            live.recv().await.unwrap(),
            AgentEvent::Error(text) if text.contains("model call failed")
        ) {
            break;
        }
    }
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;

    // The durable entry exists with the root error text.
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "error-context")
        .await
        .unwrap();
    let errors: Vec<&SessionEntry> = loaded
        .entries
        .iter()
        .filter(|entry| matches!(entry, SessionEntry::Error { .. }))
        .collect();
    assert_eq!(errors.len(), 1, "one root error, one entry: {loaded:?}");
    assert!(
        matches!(errors[0], SessionEntry::Error { text } if text.contains("provider exploded"))
    );

    // A follow-up turn succeeds; its context must not contain the error text.
    handle.prompt("retry");
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "recovered" => break,
            AgentEvent::Error(text) => panic!("follow-up turn failed: {text}"),
            _ => {}
        }
    }
    let contexts = calls.lock().unwrap();
    assert_eq!(contexts.len(), 2, "failed call + follow-up call");
    let serialized = serde_json::to_string(&contexts[1]).unwrap();
    assert!(
        !serialized.contains("provider exploded"),
        "error text leaked into the next provider context: {serialized}"
    );
    drop(handle);
    drop(task);
}

#[tokio::test]
async fn finish_when_idle_failed_turn_persists_exactly_one_error() {
    // FinishWhenIdle pins the failure through terminate(Failed): exactly
    // one durable Error entry for the single root error — no duplicate from
    // the WaitForInput-style inline emit, no recursion.
    let temp = tempfile::tempdir().unwrap();
    let (agent, _, _) = controlled(vec![Err(anyhow::anyhow!("terminal boom"))], false);
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "finish-fail".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let task = runner.start(Some("initial".into()));
    let mut status = handle.status();
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert!(matches!(
        result,
        SessionStatus::Finished(SessionResult::Failed(text)) if text.contains("terminal boom")
    ));
    task.join().await.unwrap();
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "finish-fail")
        .await
        .unwrap();
    let errors: Vec<&SessionEntry> = loaded
        .entries
        .iter()
        .filter(|entry| matches!(entry, SessionEntry::Error { .. }))
        .collect();
    assert_eq!(errors.len(), 1, "exactly one Error entry: {loaded:?}");
    assert!(matches!(errors[0], SessionEntry::Error { text } if text.contains("terminal boom")));
}

#[tokio::test]
async fn commit_error_fallback_emits_without_recursion_when_append_fails() {
    // No injectable failing store exists in this codebase (SessionStore is
    // a plain enum with no test seam, and the task forbids inventing one),
    // so the fallback is exercised with a real JSONL store whose target
    // path is a directory: every append fails, including the Error entry
    // append. commit_error must still fan out the AgentEvent::Error, must
    // not retry or recurse (exactly one event, task completes), and the
    // session still finalizes as Failed.
    let temp = tempfile::tempdir().unwrap();
    // `session_path` resolves to `<root>/.e-agent/sessions/<name>.jsonl`;
    // pre-create a DIRECTORY at that path so OpenOptions::append fails.
    let sessions_dir = temp.path().join(".e-agent/sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    std::fs::create_dir_all(sessions_dir.join("fallback.jsonl")).unwrap();

    let (agent, _, _) = controlled(vec![Err(anyhow::anyhow!("store broken"))], false);
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "fallback".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let (_, mut live, status) = handle.attach();
    let task = runner.start(Some("initial".into()));
    // The prompt append fails -> terminate(Failed) -> commit_error append
    // also fails -> fallback emit. Exactly one Error event, then finish.
    let mut errors = 0;
    loop {
        if let AgentEvent::Error(_) = live.recv().await.unwrap() {
            errors += 1;
        }
        if matches!(*status.borrow(), SessionStatus::Finished(_)) {
            break;
        }
    }
    assert_eq!(errors, 1, "fallback emits the error exactly once");
    assert!(matches!(
        *status.borrow(),
        SessionStatus::Finished(SessionResult::Failed(_))
    ));
    task.join().await.unwrap();
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
            vec![Ok("answer".into()), Ok("The user asked an earlier question and the assistant replied; the conversation then moved on to the latest request, which is still being worked on.".into())]
        } else {
            vec![Ok("The user asked an earlier question and the assistant replied; the conversation then moved on to the latest request, which is still being worked on.".into()), Ok("answer".into())]
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
                .position(|entry| matches!(entry, SessionEntry::Compaction { summary, .. } if summary == "The user asked an earlier question and the assistant replied; the conversation then moved on to the latest request, which is still being worked on."))
                .unwrap();
        assert_eq!(prompt < compact, prompt_first);
        task.join().await.unwrap();
    }
}

#[tokio::test]
async fn consecutive_compacts_are_not_folded() {
    let temp = tempfile::tempdir().unwrap();
    let (mut agent, _, _) = controlled(vec![Ok("The user asked an earlier question and the assistant replied; the conversation then moved on to the latest request, which is still being worked on.".into())], false);
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
                |event| matches!(event, AgentEvent::Notice(text) if text == "compacted: The user asked an earlier question and the assistant replied; the conversation then moved on to the latest request, which is still being worked on.")
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
            // Keeps the background-completion channel open (see
            // `recovering_agent`).
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
    // The tool result is committed before the racing release (Cancel) is
    // processed — the completed output is never lost. The release then
    // stops the turn: with nothing queued, the FinishWhenIdle session
    // finalizes Cancelled right here (no "cancelled but waiting forever"
    // intermediate state). The "turn cancelled" notice is emitted only
    // after the tool entry commit, so it pins the commit-before-cancel
    // order.
    loop {
        if matches!(
            live.recv().await.unwrap(),
            AgentEvent::Notice(text) if text == "turn cancelled"
        ) {
            break;
        }
    }
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
                    ..Default::default()

                    }),
                ),
                (
                    AssistantMessage {
                        content: Some("The user asked an earlier question and the assistant replied; the conversation then moved on to the latest request, which is still being worked on.".into()),
                        tool_calls: vec![],
                        reasoning: None,
                    },
                    Some(Usage {
                        input_tokens: 900,
                        output_tokens: 20,
                    ..Default::default()

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
                    ..Default::default()

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
    let mut prior_history = vec![
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
    ];
    // Keep enough completed activity before the current prompt that the
    // second high-usage round has genuinely replaceable context after the
    // first compaction. The test still exercises the latch, not a no-op.
    for index in 0..15 {
        let id = format!("prior-{index}");
        prior_history.push(
            Message::Assistant(AssistantMessage {
                content: None,
                tool_calls: vec![ToolCall {
                    id: id.clone(),
                    name: "keep_alive".into(),
                    arguments: "{}".into(),
                }],
                reasoning: None,
            })
            .into(),
        );
        prior_history.push(
            Message::Tool {
                call_id: id,
                name: "keep_alive".into(),
                content: "ok".into(),
                images: vec![],
                is_error: false,
                synthetic: false,
            }
            .into(),
        );
    }
    agent.restore_history(prior_history);
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
                if content == "[compacted summary of earlier conversation]\nThe user asked an earlier question and the assistant replied; the conversation then moved on to the latest request, which is still being worked on."
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
async fn successful_auto_compact_resets_latch_and_fires_again_on_next_big_round() {
    // Regression for the permanent auto-compact lockout: a SUCCESSFUL
    // compaction keeps the pre-compaction usage baseline
    // (refresh_context=false), so `record_usage`'s 80% reset condition can
    // never fire while the stale baseline stays ≥80%. The runner must clear
    // the `auto_compacted` latch on success (`clear_auto_compacted`); the
    // end of the NEXT regular round then re-evaluates the fresh baseline.
    // Model call plan (window 1000):
    //   [0] round 1 usage 800 (80%)          -> fires auto-compact #1
    //   [1] compaction #1 summary usage 900  (refresh=false, baseline stays 800)
    //   [2] round 2 usage 850 (still ≥80%)   -> must fire auto-compact #2 again
    //   [3] compaction #2 summary usage 900
    //   [4] round 3 usage 100 (<80%)         -> silent, final answer
    // Without the latch reset, round 2's check is suppressed: only 4 model
    // calls and a single compaction entry would be observed.
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
                    Some(Usage {
                        input_tokens: 800,
                        output_tokens: 10,
                        ..Default::default()
                    }),
                ),
                (
                    AssistantMessage {
                        content: Some("First compaction summary: the earlier exchange is condensed into this summary.".into()),
                        tool_calls: vec![],
                        reasoning: None,
                    },
                    Some(Usage {
                        input_tokens: 900,
                        output_tokens: 20,
                        ..Default::default()
                    }),
                ),
                (
                    AssistantMessage {
                        content: None,
                        // A materially large second activity batch makes
                        // the second compaction replace the completed first
                        // batch rather than becoming a short-turn no-op.
                        tool_calls: (0..21)
                            .map(|index| ToolCall {
                                id: format!("call-2-{index}"),
                                name: "keep_alive".into(),
                                arguments: "{}".into(),
                            })
                            .collect(),
                        reasoning: None,
                    },
                    Some(Usage {
                        input_tokens: 850,
                        output_tokens: 10,
                        ..Default::default()
                    }),
                ),
                (
                    AssistantMessage {
                        content: Some("Second compaction summary: the current turn is still large, so the conversation is compacted once more.".into()),
                        tool_calls: vec![],
                        reasoning: None,
                    },
                    Some(Usage {
                        input_tokens: 900,
                        output_tokens: 20,
                        ..Default::default()
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
                        ..Default::default()
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
        "auto-compact-relatch".into(),
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

    {
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            5,
            "expected round, compaction, round, compaction, round: {calls:?}"
        );
    }

    // Two persisted Compaction entries, and the FINAL round's derived
    // context opens with the second compaction's summary — proving the
    // second auto-compact ran on top of the first (its retained tail starts
    // at the current prompt, so the first summary lives in the compacted
    // earlier part, not in the second entry's retained slice).
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "auto-compact-relatch")
        .await
        .unwrap();
    let compactions: Vec<&SessionEntry> = loaded
        .entries
        .iter()
        .filter(|entry| matches!(entry, SessionEntry::Compaction { .. }))
        .collect();
    assert_eq!(compactions.len(), 2, "auto-compact must fire twice");
    {
        let calls = calls.lock().unwrap();
        assert!(matches!(
            &calls[4][0],
            Message::User { content, .. }
                if content.contains("Second compaction summary")
        ));
    }
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
    // The in-flight compaction is preempted (released): no entry, no
    // projection. With nothing queued the FinishWhenIdle session finalizes
    // Cancelled right here — no "cancelled but waiting forever" state. The
    // "compaction cancelled" notice is emitted only after the operation is
    // dropped, pinning the order.
    loop {
        if matches!(
            live.recv().await.unwrap(),
            AgentEvent::Notice(text) if text == "compaction cancelled"
        ) {
            break;
        }
    }
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
    // A cancel is a Notice, never an Error entry: nothing to audit-persist.
    assert!(
        !loaded
            .entries
            .iter()
            .any(|entry| matches!(entry, SessionEntry::Error { .. })),
        "cancelled compaction must not persist an Error entry: {loaded:?}"
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
        // Keeps the background-completion channel open (see
        // `recovering_agent`).
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
    // The round completed naturally (final answer, no tool calls) and its
    // output is committed before the racing release is processed — the
    // completed result wins, so the session finalizes Completed, never
    // Cancelled, and no "turn cancelled" notice is emitted (nothing was
    // preempted).
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "completed answer" => break,
            AgentEvent::Notice(text) if text == "turn cancelled" => {
                panic!("a completed round must not be reported as cancelled")
            }
            _ => {}
        }
    }
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert_eq!(
        result,
        SessionStatus::Finished(SessionResult::Completed(Some("completed answer".into())))
    );
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
            reply: Some("The completed summary of the earlier conversation covers the earlier exchange between the user and the assistant; no unfinished work remains.".into()),
            commands: commands.clone(),
        }),
        // Keeps the background-completion channel open (see
        // `recovering_agent`).
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
    // The completed compaction is durably committed (projection notice
    // pinned) before the racing release is processed; the release then ends
    // the FinishWhenIdle session as Cancelled (no queued prompt) — the
    // committed compaction output is never lost.
    loop {
        if matches!(
            live.recv().await.unwrap(),
            AgentEvent::Notice(text) if text == "compacted: The completed summary of the earlier conversation covers the earlier exchange between the user and the assistant; no unfinished work remains."
        ) {
            break;
        }
    }
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
                .filter(|entry| matches!(entry, SessionEntry::Compaction { summary, .. } if summary == "The completed summary of the earlier conversation covers the earlier exchange between the user and the assistant; no unfinished work remains."))
                .count(),
            1
        );
    assert_eq!(
            handle
                .snapshot()
                .iter()
                .filter(|event| matches!(event, AgentEvent::Notice(text) if text == "compacted: The completed summary of the earlier conversation covers the earlier exchange between the user and the assistant; no unfinished work remains."))
                .count(),
            1
        );
    drop(handle);
    drop(task);
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
async fn runner_commits_image_bearing_tool_and_strips_requests_without_vision() {
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

    // Canonical persisted Tool entry keeps the summary AND the image
    // reference (no marker, no synthetic user message, no skip annotation
    // in history — that only appears on the non-vision request copy).
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "marker-novision")
        .await
        .unwrap();
    let tool = loaded
        .entries
        .iter()
        .find_map(|entry| match entry {
            SessionEntry::Message {
                message: Message::Tool {
                    content, images, ..
                },
            } => Some((content.clone(), images.clone())),
            _ => None,
        })
        .unwrap();
    assert!(tool.0.starts_with("[image read: pic.png]"));
    assert!(!tool.0.contains("已跳过附加"));
    assert!(!tool.0.contains("__EA_IMAGE__"));
    assert_eq!(
        tool.1,
        vec![crate::agent::ImagePart {
            hash: "hash123".into(),
            mime: "image/png".into(),
        }]
    );
    // No synthetic user message ever.
    assert!(!loaded.entries.iter().any(|entry| match entry {
        SessionEntry::Message {
            message: Message::User { content, .. },
        } => content.starts_with("[image attached:"),
        _ => false,
    }));
    // And no UserPrompt event projected a synthetic image user.
    assert!(!handle.snapshot().iter().any(|event| matches!(
        event,
        AgentEvent::UserPrompt(prompt) if prompt.starts_with("[image attached:")
    )));
    // The second model call saw no images (request copy stripped), with the
    // tool content text-degraded by the skip note.
    let calls = calls.lock().unwrap();
    assert!(
        calls[1]
            .iter()
            .all(|message| !matches!(message, Message::User { images, .. } if !images.is_empty()))
    );
    assert!(
        calls[1]
            .iter()
            .all(|message| !matches!(message, Message::Tool { images, .. } if !images.is_empty()))
    );
    assert!(calls[1].iter().any(|message| matches!(
        message,
        Message::Tool { content, .. } if content.contains("已跳过附加")
    )));
}

/// A tool whose result is far larger than the projection budget — the
/// durable-before-ref scenario for oversized persisted tool content.
struct HugeOutputTool;

#[async_trait]
impl Tool for HugeOutputTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "huge".into(),
            description: "returns a huge result".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }
    async fn execute(&self, _: Value) -> Result<ToolOutput, String> {
        Ok(ToolOutput::text(format!(
            "{}{}{}",
            "h".repeat(crate::output_receipt::PROJECTION_HEAD_BYTES),
            "MIDDLE-SECRET",
            "t".repeat(crate::output_receipt::PROJECTION_TAIL_BYTES)
        )))
    }
}

/// Durable-before-ref: a receipt only ever appears in a provider request
/// AFTER the oversized field was durably appended — the embedded `eout1`
/// ref must resolve against the persisted session file (never against
/// in-memory-only state), and the persisted field stays FULL.
#[tokio::test]
async fn runner_durable_before_ref_for_oversized_tool_content() {
    let temp = tempfile::tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::new(
        Box::new(ScriptedContextCaptureModel {
            replies: vec![
                (
                    AssistantMessage {
                        content: None,
                        tool_calls: vec![ToolCall {
                            id: "call-huge".into(),
                            name: "huge".into(),
                            arguments: r#"{}"#.into(),
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
        vec![Box::new(HugeOutputTool)],
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "durable-ref".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let task = runner.start(Some("run it".into()));
    let mut status = handle.status();
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert_eq!(
        result,
        SessionStatus::Finished(SessionResult::Completed(Some("done".into())))
    );
    task.join().await.unwrap();

    // The persisted file holds the FULL tool result (lossless).
    let store = SessionStore::Jsonl;
    let loaded = store.load(temp.path(), "durable-ref").await.unwrap();
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
    assert!(tool.contains("MIDDLE-SECRET"), "persisted field stays full");

    // The SECOND provider request carries the bounded projection with an
    // embedded receipt (the first request preceded the tool result).
    let bounded = {
        let requests = calls.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let bounded = requests[1]
            .iter()
            .find_map(|message| match message {
                Message::Tool { content, .. } if content.contains("ref=eout1.") => {
                    Some(content.clone())
                }
                _ => None,
            })
            .expect("second request must carry the bounded tool result with a receipt");
        assert!(bounded.starts_with(&"h".repeat(crate::output_receipt::PROJECTION_HEAD_BYTES)));
        assert!(bounded.ends_with(&"t".repeat(crate::output_receipt::PROJECTION_TAIL_BYTES)));
        assert!(!bounded.contains("MIDDLE-SECRET"));
        bounded
    };

    // The embedded receipt resolves against the DURABLY PERSISTED session
    // file (durable-before-ref): read the exact full field back.
    let ref_start = bounded.find("ref=eout1.").unwrap() + "ref=".len();
    let ref_end = bounded[ref_start..].find(']').unwrap() + ref_start;
    let receipt = &bounded[ref_start..ref_end];
    let parsed = crate::output_receipt::parse_ref(receipt).unwrap();
    let crate::output_receipt::OutputRef::Direct { entry_id, field } = parsed else {
        panic!("expected a direct short ref, got legacy: {receipt}");
    };
    assert_eq!(field, crate::output_receipt::FieldId::ToolContent);
    let bytes = store
        .read_field_direct(temp.path(), "durable-ref", entry_id, field)
        .await
        .unwrap();
    assert_eq!(
        bytes,
        tool.as_bytes(),
        "receipt reads the persisted full field"
    );
}

#[tokio::test]
async fn runner_commits_image_bearing_tool_with_vision_model() {
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

    // Persisted tool entry keeps the summary AND the image reference; no
    // synthetic user message follows it.
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "marker-vision")
        .await
        .unwrap();
    let tool = loaded
        .entries
        .iter()
        .find_map(|entry| match entry {
            SessionEntry::Message {
                message: Message::Tool {
                    content, images, ..
                },
            } => Some((content.clone(), images.clone())),
            _ => None,
        })
        .unwrap();
    assert!(tool.0.starts_with("[image read: pic.png]"));
    assert!(!tool.0.contains("__EA_IMAGE__"));
    assert!(!tool.0.contains("已跳过附加"));
    assert_eq!(
        tool.1,
        vec![crate::agent::ImagePart {
            hash: "hash123".into(),
            mime: "image/png".into(),
        }]
    );
    assert!(!loaded.entries.iter().any(|entry| match entry {
        SessionEntry::Message {
            message: Message::User { content, .. },
        } => content.starts_with("[image attached:"),
        _ => false,
    }));
    // The second model call saw the image on the Tool message.
    let calls = calls.lock().unwrap();
    assert!(calls[1].iter().any(|message| matches!(
        message,
        Message::Tool { images, .. } if !images.is_empty()
    )));
}

#[tokio::test]
async fn switch_to_non_vision_during_read_image_strips_request() {
    // Race: the session starts on a vision model, calls read_image, and the
    // user switches to a non-vision model WHILE the tool is executing. The
    // canonical history keeps the image-bearing Tool entry either way, but
    // the next (non-vision) model call — which simulates the wire vision
    // gate by erroring on image-bearing requests — must see a stripped
    // request and succeed.
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
    // Canonical history keeps the image-bearing Tool entry — no synthetic
    // user message, no in-history skip annotation.
    assert!(!loaded.entries.iter().any(|entry| match entry {
        SessionEntry::Message {
            message: Message::User { content, .. },
        } => content.starts_with("[image attached:"),
        _ => false,
    }));
    let tool = loaded
        .entries
        .iter()
        .find_map(|entry| match entry {
            SessionEntry::Message {
                message: Message::Tool {
                    content, images, ..
                },
            } => Some((content.clone(), images.clone())),
            _ => None,
        })
        .unwrap();
    assert!(tool.0.starts_with("[image read: pic.png]"));
    assert!(!tool.0.contains("已跳过附加"));
    assert_eq!(tool.1.len(), 1);
    // The non-vision model's request was stripped (it would have errored on
    // an image-bearing request — the session was not poisoned) and the tool
    // content was text-degraded on the request copy.
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls[1].iter().all(|message| !matches!(
        message,
        Message::User { images, .. } if !images.is_empty()
    )));
    assert!(calls[1].iter().all(|message| !matches!(
        message,
        Message::Tool { images, .. } if !images.is_empty()
    )));
    assert!(calls[1].iter().any(|message| matches!(
        message,
        Message::Tool { content, .. } if content.contains("已跳过附加")
    )));
}

#[tokio::test]
async fn switch_to_vision_during_read_image_keeps_history_image_and_serves_it() {
    // Inverse race: session starts on a non-vision model, calls read_image,
    // and the user switches to a vision model WHILE the tool is executing.
    // The canonical history keeps the image-bearing Tool entry, and the new
    // vision model's next request sees the image.
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
    // The canonical image-bearing Tool entry was committed and carries the
    // image; no synthetic user message exists.
    let tool = loaded
        .entries
        .iter()
        .find_map(|entry| match entry {
            SessionEntry::Message {
                message: Message::Tool {
                    content, images, ..
                },
            } => Some((content.clone(), images.clone())),
            _ => None,
        })
        .unwrap();
    assert!(tool.0.starts_with("[image read: pic.png]"));
    assert_eq!(
        tool.1,
        vec![crate::agent::ImagePart {
            hash: "hash123".into(),
            mime: "image/png".into(),
        }]
    );
    assert!(!loaded.entries.iter().any(|entry| match entry {
        SessionEntry::Message {
            message: Message::User { content, .. },
        } => content.starts_with("[image attached:"),
        _ => false,
    }));
    // The vision model's next request saw the image on the Tool message.
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls[1].iter().any(|message| matches!(
        message,
        Message::Tool { images, .. } if !images.is_empty()
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
    async fn execute(&self, _: Value) -> Result<ToolOutput, String> {
        Ok(ToolOutput::text(format!(
            "started background task {}: {}",
            self.id, self.label
        )))
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
    dropped: Option<Arc<Notify>>,
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
        // Pop the reply BEFORE blocking: if the blocked call is preempted
        // (dropped by a release/cancel), the reply is already consumed and
        // the next call pops the following one. Popping after the await
        // would leak the popped reply back into the queue, so the next call
        // would repeat it (the steer batch test hangs on exactly that).
        let reply = self.replies.pop_front().expect("unexpected model call");
        if self.call_count == self.block_call {
            // While blocked, hold a drop probe so a test can wait for the
            // release (cancel) to actually preempt this in-flight future
            // before releasing the block — otherwise the round may complete
            // naturally and the release becomes stale (see the steer batch
            // test, which needs the preemption path to be deterministic).
            let _probe = self
                .dropped
                .as_ref()
                .map(|dropped| DropProbe(dropped.clone()));
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok((reply, None))
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
            started_at_ms: None,
            duration_ms: None,
            exit_code: None,
            signal: None,
            status: None,
            kind: None,
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
            started_at_ms: None,
            duration_ms: None,
            exit_code: None,
            signal: None,
            status: None,
            kind: None,
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
            dropped: None,
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
            started_at_ms: None,
            duration_ms: None,
            exit_code: None,
            signal: None,
            status: None,
            kind: None,
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

/// Bash tool that sends its own background completion from inside
/// `execute`, so the completion is in the agent's channel while the tool
/// is still running (deterministic mid-tool delivery).
struct SelfCompletingBash {
    id: u64,
    sender: Arc<Mutex<Option<mpsc::UnboundedSender<AgentEvent>>>>,
}

#[async_trait]
impl Tool for SelfCompletingBash {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "test only".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }
    async fn execute(&self, _: Value) -> Result<ToolOutput, String> {
        self.sender
            .lock()
            .unwrap()
            .as_ref()
            .expect("agent must wire the bash completion sender")
            .send(AgentEvent::BackgroundCompleted {
                id: self.id,
                output: "build ok".into(),
                label: Some("cargo build".into()),
                started_at_ms: None,
                duration_ms: None,
                exit_code: None,
                signal: None,
                status: None,
                kind: None,
            })
            .unwrap();
        Ok(ToolOutput::text(format!(
            "started background task {}: cargo build",
            self.id
        )))
    }
    fn set_event_sender(&mut self, sender: mpsc::UnboundedSender<AgentEvent>) {
        *self.sender.lock().unwrap() = Some(sender);
    }
}

#[tokio::test]
async fn completion_arriving_during_tool_batch_is_visible_to_next_provider_call_without_followup() {
    // A completion delivered while the tool batch executes must be
    // committed after the batch's last Tool result, before the next
    // provider call sees it — no follow-up turn starts.
    let temp = tempfile::tempdir().unwrap();
    let store = SessionStore::Jsonl;
    let sender = Arc::new(Mutex::new(None));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::new(
        Box::new(ScriptedContextCaptureModel {
            replies: vec![
                (
                    AssistantMessage {
                        content: None,
                        tool_calls: vec![background_bash_call("cargo build", false)],
                        reasoning: None,
                    },
                    None,
                ),
                (
                    AssistantMessage {
                        content: Some("all done".into()),
                        tool_calls: vec![],
                        reasoning: None,
                    },
                    None,
                ),
            ]
            .into(),
            calls: calls.clone(),
        }),
        vec![Box::new(SelfCompletingBash {
            id: 9,
            sender: sender.clone(),
        })],
    );
    let (runner, _) = SessionRunner::new(
        agent,
        store.clone(),
        temp.path().into(),
        "mid-turn-bg".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let task = runner.start(Some("go".into()));

    // A clean join proves exactly two model calls (a regression would panic on a third).
    task.join().await.unwrap();

    // Persisted order: Assistant(tool_calls) -> Tool -> BackgroundCompletion.
    let loaded = store.load(temp.path(), "mid-turn-bg").await.unwrap();
    let order: Vec<&str> = loaded
        .entries
        .iter()
        .map(|entry| match entry {
            SessionEntry::BackgroundCompletion { .. } => "completion",
            SessionEntry::Message { message, .. } => match message {
                Message::Assistant(m) if !m.tool_calls.is_empty() => "assistant",
                Message::Tool { .. } => "tool",
                _ => "other",
            },
            _ => "other",
        })
        .collect();
    assert_eq!(order, ["other", "assistant", "tool", "completion", "other"]);

    let calls = calls.lock().unwrap();
    assert!(
        calls.len() == 2
            && calls[1].iter().any(
                |m| matches!(m, Message::User { content, .. } if content.contains("build ok"))
            ),
        "no extra follow-up turn; next call must see the completion: {calls:?}"
    );
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

#[tokio::test]
async fn finish_when_idle_waits_indefinitely_for_blocking_completion() {
    // Control: without a finite timeout, the session
    // must NOT finalize early while a blocking background task runs; it
    // only finalizes after the completion is delivered and the model runs
    // its final round.
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
        "finalize-wait-none".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(Some("run build".into()));

    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "build started, waiting" => break,
            AgentEvent::Error(text) => panic!("turn failed: {text}"),
            _ => {}
        }
    }
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;
    // A short window must pass without any finalize.
    let early = tokio::time::timeout(
        std::time::Duration::from_millis(400),
        wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))),
    )
    .await;
    assert!(
        early.is_err(),
        "the session must keep waiting for the background task"
    );

    // Deliver the completion: normal path resumes and finalizes.
    sender
        .lock()
        .unwrap()
        .as_ref()
        .expect("agent must wire the bash completion sender")
        .send(AgentEvent::BackgroundCompleted {
            id: 9,
            output: "build ok".into(),
            label: Some("cargo build".into()),
            started_at_ms: None,
            duration_ms: None,
            exit_code: None,
            signal: None,
            status: None,
            kind: None,
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
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Steering v2 — step 1: lock the new release contract with tests
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
//
// NEW CONTRACT (oracle-approved; step 2 will rework the runner, this step
// only defines the behavior):
//
//   cancel == release. A Cancel command never terminates the subagent and
//   never leaves a persistent "cancelled" state. It preempts the CURRENT
//   operation (the in-flight future is dropped) so queued messages are
//   processed immediately; whether the subagent ends is decided solely by
//   whether the turns started after the release complete naturally.
//
//   1. Busy + queued prompt -> release -> in-flight future dropped (its
//      output never committed) -> queued prompt durably committed
//      (PromptConsumed + UserPrompt) -> new turn completes naturally ->
//      Finished(Completed).
//   2. Multiple queued prompts are consumed in ONE batch (one model call,
//      FIFO order).
//   3. Release racing an operation's own completion: the completed output
//      is committed first — never lost — and with nothing queued the
//      FinishWhenIdle session then finalizes (Completed, since the turn
//      already ended naturally) instead of parking in a stale state.
//   4. Release during a tool: the interrupted tool call is never committed;
//      the next provider context synthesizes an error result for it
//      (repair_tool_pairs), so tool pairs stay legal on the wire.
//   5. Release during compaction: no Compaction entry, no projection.
//   6. Release with NO queued prompt: no cross-turn "cancelled keep-alive"
//      state. WaitForInput returns to Idle (and stays usable); FinishWhenIdle
//      finalizes Cancelled directly (an emergency cancel must not linger).
//   7. WaitForInput ("btw") processes queued prompts exactly like
//      FinishWhenIdle, but returns to Idle afterwards — it never finishes
//      naturally.
//   8. Hard termination (DELETE / task-panel cancel) is the runner-task
//      abort and keeps aborting the in-flight operation immediately
//      (runner-level pin here; end-to-end pin in
//      server::delete_subagent_session_aborts_delegate_runner_and_cleans_up).
//   9. Sync parent abandonment still aborts the child runner (pinned in
//      delegate_tests::sync_abandon_aborts_runner_and_cleans_session).
//
// STATUS under the CURRENT implementation (step 2 implemented: the
// cross-turn `cancelled` flag is gone — a Cancel is a local release
// handled at each operation boundary):
//   * ALL 10 steer_* tests are GREEN, including the three that were RED
//     under step 1's old keep-alive semantics
//     (steer_release_*without_queued_prompt_finish_when_idle* and
//     steer_release_racing_round_completion_*).
//   * OLD-contract tests removed/reworked in step 2:
//     - deleted: cancel_keeps_finish_when_idle_session_alive_for_follow_up,
//       cancel_then_compact_keeps_finish_when_idle_session_alive_for_follow_up
//       (they asserted "cancel keeps the FinishWhenIdle session alive for
//       follow-up" — the exact "infinite keep-alive" state step 2 removed).
//     - reworked to the stale-release contract (commit-before-release
//       assertions kept, final state changed from "Idle and stays alive" to
//       Finished): completed_tool_result_is_committed_before_stale_cancel
//       (Finished(Cancelled)), in_flight_compaction_cancel_has_no_entry_or_projection
//       (Finished(Cancelled)), completed_round_is_committed_before_stale_cancel
//       (Finished(Completed) — the round completed naturally, the release is
//       stale), completed_compaction_is_committed_before_stale_cancel
//       (Finished(Cancelled)).
//   * cancel_with_queued_message_keeps_processing_it keeps its observable
//     behavior and matches contract 1 — kept as-is.

/// Blocks inside the tool until released; used to hold a tool execution in
/// flight while a release (cancel) is delivered.
struct BlockingNoopTool {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl Tool for BlockingNoopTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "blocker".into(),
            description: "test only".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }
    async fn execute(&self, _: Value) -> Result<ToolOutput, String> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(ToolOutput::text("blocked tool result"))
    }
}

/// Preemptible model: the first call blocks until released while holding a
/// drop probe, so a test can observe the in-flight future being dropped by
/// a release (cancel) — proof of preemption. Later calls serve `replies`
/// normally.
struct PreemptibleModel {
    replies: VecDeque<anyhow::Result<AssistantMessage>>,
    block_first: bool,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    dropped: Arc<Notify>,
}

#[async_trait]
impl Model for PreemptibleModel {
    async fn complete(
        &mut self,
        _: &[Message],
        _: &[ToolSpec],
        _: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        let reply = self.replies.pop_front().expect("unexpected model call");
        if self.block_first {
            self.block_first = false;
            let _probe = DropProbe(self.dropped.clone());
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok((reply?, None))
    }
}

#[tokio::test]
async fn steer_release_preempts_in_flight_round_and_runs_queued_prompt_to_natural_completion() {
    // Contract 1: Busy + queued prompt -> release -> the in-flight model
    // future is dropped (preempted, its output never committed) -> the
    // queued prompt is durably committed with PromptConsumed + UserPrompt ->
    // the new turn completes naturally -> Finished(Completed).
    let temp = tempfile::tempdir().unwrap();
    let (entered, release, dropped) = (
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
    );
    let agent = Agent::new(
        Box::new(PreemptibleModel {
            replies: vec![
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
            ]
            .into(),
            block_first: true,
            entered: entered.clone(),
            release: release.clone(),
            dropped: dropped.clone(),
        }),
        vec![Box::new(KeepAliveTool { sender: None })],
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "steer-release-preempt".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(Some("initial".into()));

    entered.notified().await;
    handle.prompt("queued prompt");
    handle.cancel();

    // The release drops the in-flight model future before it can produce
    // output (the probe notifies exactly when the drop lands).
    dropped.notified().await;
    release.notify_one();

    loop {
        match live.recv().await.unwrap() {
            AgentEvent::Notice(text) if text == "turn cancelled" => break,
            _ => {}
        }
    }
    // The queued prompt is consumed (PromptConsumed) and committed
    // (UserPrompt) before the new turn starts.
    let mut saw_consumed = false;
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::PromptConsumed => saw_consumed = true,
            AgentEvent::UserPrompt(text) if text == "queued prompt" && saw_consumed => break,
            AgentEvent::UserPrompt(_) => panic!("UserPrompt arrived before PromptConsumed"),
            _ => {}
        }
    }
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "queued answer" => break,
            _ => {}
        }
    }
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert_eq!(
        result,
        SessionStatus::Finished(SessionResult::Completed(Some("queued answer".into())))
    );
    task.join().await.unwrap();

    let loaded = SessionStore::Jsonl
        .load(temp.path(), "steer-release-preempt")
        .await
        .unwrap();
    assert!(
        !loaded.entries.iter().any(|entry| matches!(
            entry,
            SessionEntry::Message {
                message: Message::Assistant(message)
            } if message.content.as_deref() == Some("interrupted")
        )),
        "a preempted round must never commit its output"
    );
    assert!(loaded.entries.iter().any(|entry| matches!(
        entry,
        SessionEntry::Message {
            message: Message::User { content, .. }
        } if content == "queued prompt"
    )));
    assert!(loaded.entries.iter().any(|entry| matches!(
        entry,
        SessionEntry::Message {
            message: Message::Assistant(message)
        } if message.content.as_deref() == Some("queued answer")
    )));
}

#[tokio::test]
async fn steer_release_batches_multiple_queued_prompts_into_one_turn() {
    // Contract 2: several prompts queued while busy are consumed in ONE
    // batch after the release — one model call whose context joins them in
    // FIFO order.
    let temp = tempfile::tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let dropped = Arc::new(Notify::new());
    let agent = Agent::new(
        Box::new(BlockingContextCaptureModel {
            replies: vec![
                AssistantMessage {
                    content: Some("interrupted".into()),
                    tool_calls: Vec::new(),
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("batch answer".into()),
                    tool_calls: Vec::new(),
                    reasoning: None,
                },
            ]
            .into(),
            block_call: 1,
            entered: entered.clone(),
            release: release.clone(),
            dropped: Some(dropped.clone()),
            calls: calls.clone(),
            call_count: 0,
        }),
        vec![Box::new(KeepAliveTool { sender: None })],
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "steer-batch".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(Some("initial".into()));

    entered.notified().await;
    handle.prompt("first queued");
    handle.prompt("second queued");
    handle.prompt("third queued");
    handle.cancel();
    // Wait until the release has actually dropped the in-flight model
    // future (the drop probe fires only on preemption) before releasing the
    // block — otherwise the round could complete naturally and the release
    // would go stale (no "turn cancelled", wrong path for this contract).
    dropped.notified().await;
    release.notify_one(); // the future is gone; harmless

    loop {
        match live.recv().await.unwrap() {
            AgentEvent::Notice(text) if text == "turn cancelled" => break,
            _ => {}
        }
    }
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "batch answer" => break,
            _ => {}
        }
    }
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert_eq!(
        result,
        SessionStatus::Finished(SessionResult::Completed(Some("batch answer".into())))
    );
    task.join().await.unwrap();

    let calls = calls.lock().unwrap();
    assert_eq!(
        calls.len(),
        2,
        "preempted round + one batched turn: {calls:?}"
    );
    let users: Vec<&str> = calls[1]
        .iter()
        .filter_map(|message| match message {
            Message::User { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        users,
        vec!["initial", "first queued\n\nsecond queued\n\nthird queued"],
        "all queued prompts must be joined FIFO in a single batch"
    );
}

#[tokio::test]
async fn steer_release_racing_round_completion_commits_answer_then_finalizes() {
    // Contract 3: when the release races the round's own completion, the
    // completed output is committed first (never lost). With nothing queued
    // the FinishWhenIdle session then finalizes with that completed answer
    // instead of parking in a stale "cancelled" Idle state.
    let temp = tempfile::tempdir().unwrap();
    let commands = Arc::new(Mutex::new(None));
    let agent = Agent::new(
        Box::new(CompletingWithCancelModel {
            reply: Some("completed answer".into()),
            commands: commands.clone(),
        }),
        vec![Box::new(KeepAliveTool { sender: None })],
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "steer-race-round".into(),
        IdlePolicy::FinishWhenIdle,
    );
    *commands.lock().unwrap() = Some(handle.commands.clone());
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(Some("prompt".into()));

    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "completed answer" => break,
            AgentEvent::Notice(text) if text == "turn cancelled" => break,
            _ => {}
        }
    }
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))),
    )
    .await
    .expect("FinishWhenIdle must finalize after a stale release with no queued prompt");
    assert_eq!(
        result,
        SessionStatus::Finished(SessionResult::Completed(Some("completed answer".into())))
    );
    task.join().await.unwrap();

    let loaded = SessionStore::Jsonl
        .load(temp.path(), "steer-race-round")
        .await
        .unwrap();
    assert!(loaded.entries.iter().any(|entry| matches!(
        entry,
        SessionEntry::Message {
            message: Message::Assistant(message)
        } if message.content.as_deref() == Some("completed answer")
    )));
}

#[tokio::test]
async fn steer_release_during_tool_keeps_next_provider_context_legal() {
    // Contract 4: releasing mid-tool drops the tool future; the interrupted
    // tool call is never committed, and the next provider context
    // synthesizes an error result for it (repair_tool_pairs), so the wire
    // never sees an unpaired tool_call.
    let temp = tempfile::tempdir().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let agent = Agent::new(
        Box::new(ScriptedContextCaptureModel {
            replies: vec![
                (
                    AssistantMessage {
                        content: None,
                        tool_calls: vec![ToolCall {
                            id: "call-1".into(),
                            name: "blocker".into(),
                            arguments: "{}".into(),
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
        vec![Box::new(BlockingNoopTool {
            entered: entered.clone(),
            release: release.clone(),
        })],
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "steer-tool-release".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(Some("initial".into()));

    entered.notified().await; // tool execution in flight
    handle.prompt("queued after release");
    handle.cancel();

    loop {
        match live.recv().await.unwrap() {
            AgentEvent::Notice(text) if text == "turn cancelled" => break,
            _ => {}
        }
    }
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "done" => break,
            _ => {}
        }
    }
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert_eq!(
        result,
        SessionStatus::Finished(SessionResult::Completed(Some("done".into())))
    );
    task.join().await.unwrap();

    // The guard is scoped so it is dropped before the awaited reload below.
    {
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2, "tool round + follow-up round: {calls:?}");
        let final_context = &calls[1];
        assert_eq!(
            repair_tool_pairs(final_context.clone()),
            *final_context,
            "derived context must already be a fixed point: {final_context:?}"
        );
        let tool_messages: Vec<&Message> = final_context
            .iter()
            .filter(|message| matches!(message, Message::Tool { .. }))
            .collect();
        assert_eq!(tool_messages.len(), 1);
        assert!(matches!(
            tool_messages[0],
            Message::Tool {
                call_id,
                name,
                content,
                is_error: true,
                synthetic: true,
                ..
            } if call_id == "call-1"
                && name == "blocker"
                && content == "[turn interrupted before a tool result was produced]"
        ));
    }
    // The interrupted tool call never produced a committed tool entry.
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "steer-tool-release")
        .await
        .unwrap();
    assert!(!loaded.entries.iter().any(|entry| matches!(
        entry,
        SessionEntry::Message {
            message: Message::Tool { .. }
        }
    )));
}

#[tokio::test]
async fn steer_release_during_compaction_leaves_no_projection_and_runs_queued_prompt() {
    // Contract 5: releasing during compaction drops the compaction future —
    // no Compaction entry, no "compacted:" projection — and the queued
    // prompt then opens a fresh turn that completes naturally.
    let temp = tempfile::tempdir().unwrap();
    let (mut agent, entered, _release) = recovering_agent(
        vec![
            Ok(AssistantMessage {
                content: Some("interrupted summary".into()),
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
    history_for_compaction(&mut agent);
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "steer-compact-release".into(),
        IdlePolicy::FinishWhenIdle,
    );
    handle.compact();
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(None);
    entered.notified().await;
    handle.prompt("queued during compaction");
    handle.cancel();

    loop {
        match live.recv().await.unwrap() {
            AgentEvent::Notice(text) if text == "compaction cancelled" => break,
            _ => {}
        }
    }
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::UserPrompt(text) if text == "queued during compaction" => break,
            _ => {}
        }
    }
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "queued answer" => break,
            _ => {}
        }
    }
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert_eq!(
        result,
        SessionStatus::Finished(SessionResult::Completed(Some("queued answer".into())))
    );
    task.join().await.unwrap();

    let loaded = SessionStore::Jsonl
        .load(temp.path(), "steer-compact-release")
        .await
        .unwrap();
    assert!(
        !loaded
            .entries
            .iter()
            .any(|entry| matches!(entry, SessionEntry::Compaction { .. })),
        "a released compaction must leave no entry"
    );
    assert!(
        !handle.snapshot().iter().any(|event| matches!(
            event,
            AgentEvent::Notice(text) if text.starts_with("compacted:")
        )),
        "a released compaction must leave no projection"
    );
    assert!(loaded.entries.iter().any(|entry| matches!(
        entry,
        SessionEntry::Message {
            message: Message::User { content, .. }
        } if content == "queued during compaction"
    )));
}

#[tokio::test]
async fn steer_release_without_queued_prompt_finish_when_idle_finalizes_cancelled() {
    // Contract 6 (FinishWhenIdle side): a release with nothing queued must
    // not leave the session parked in a stale cross-turn "cancelled" Idle
    // state ("infinite keep-alive"). There is no follow-up to wait for, so
    // the session finalizes Cancelled directly at the release boundary (the
    // local `Steering::ReleasedIdle` outcome — there is no persistent
    // cross-turn "cancelled" state).
    let temp = tempfile::tempdir().unwrap();
    let (agent, entered, _release) = recovering_agent(
        vec![Ok(AssistantMessage {
            content: Some("interrupted".into()),
            tool_calls: Vec::new(),
            reasoning: None,
        })],
        true,
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "steer-release-noqueue".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let (_, mut live, _) = handle.attach();
    let task = runner.start(Some("initial".into()));
    entered.notified().await;
    handle.cancel();

    loop {
        match live.recv().await.unwrap() {
            AgentEvent::Notice(text) if text == "turn cancelled" => break,
            _ => {}
        }
    }
    let mut status = handle.status();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))),
    )
    .await
    .expect("release with no queued prompt must finalize a FinishWhenIdle session (no infinite keep-alive)");
    assert_eq!(result, SessionStatus::Finished(SessionResult::Cancelled));
    task.join().await.unwrap();

    let loaded = SessionStore::Jsonl
        .load(temp.path(), "steer-release-noqueue")
        .await
        .unwrap();
    assert!(
        !loaded.entries.iter().any(|entry| matches!(
            entry,
            SessionEntry::Message {
                message: Message::Assistant(_)
            }
        )),
        "no interrupted output may be committed"
    );
}

#[tokio::test]
async fn steer_release_during_tool_without_queued_prompt_finish_when_idle_finalizes_cancelled() {
    // Contract 6, tool path: the same no-queued-prompt release while a tool
    // is executing must also finalize Cancelled at the release boundary
    // instead of lingering at Idle.
    let temp = tempfile::tempdir().unwrap();
    let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let agent = Agent::new(
        Box::new(ScriptedAssistantModel {
            replies: vec![AssistantMessage {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "blocker".into(),
                    arguments: "{}".into(),
                }],
                reasoning: None,
            }]
            .into(),
        }),
        vec![Box::new(BlockingNoopTool {
            entered: entered.clone(),
            release: release.clone(),
        })],
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "steer-tool-noqueue".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let (_, mut live, _) = handle.attach();
    let task = runner.start(Some("initial".into()));
    entered.notified().await;
    handle.cancel();

    loop {
        match live.recv().await.unwrap() {
            AgentEvent::Notice(text) if text == "turn cancelled" => break,
            _ => {}
        }
    }
    let mut status = handle.status();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))),
    )
    .await
    .expect("release during a tool with no queued prompt must finalize (no infinite keep-alive)");
    assert_eq!(result, SessionStatus::Finished(SessionResult::Cancelled));
    task.join().await.unwrap();
}

#[tokio::test]
async fn steer_release_with_queued_compact_finish_when_idle_drops_the_compact() {
    // Contract 6 variant: a Compact queued behind an in-flight round is an
    // internal maintenance command, not a user message. When the release
    // finalizes the FinishWhenIdle session as Cancelled (nothing queued
    // user-side), the queued Compact is dropped without ever running —
    // cancel = flush applies to queued messages only.
    let temp = tempfile::tempdir().unwrap();
    let (agent, entered, _release) = recovering_agent(
        vec![Ok(AssistantMessage {
            content: Some("interrupted".into()),
            tool_calls: Vec::new(),
            reasoning: None,
        })],
        true,
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "steer-compact-drop".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let (_, mut live, _) = handle.attach();
    let task = runner.start(Some("initial".into()));
    entered.notified().await; // round in flight
    handle.compact(); // queued, never started
    handle.cancel(); // release

    loop {
        match live.recv().await.unwrap() {
            AgentEvent::Notice(text) if text == "turn cancelled" => break,
            _ => {}
        }
    }
    let mut status = handle.status();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))),
    )
    .await
    .expect("release with a queued Compact and no prompts must finalize a FinishWhenIdle session");
    assert_eq!(result, SessionStatus::Finished(SessionResult::Cancelled));
    task.join().await.unwrap();

    // The queued Compact never ran: no Compaction entry, no projection.
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "steer-compact-drop")
        .await
        .unwrap();
    assert!(
        !loaded
            .entries
            .iter()
            .any(|entry| matches!(entry, SessionEntry::Compaction { .. })),
        "a release-dropped queued Compact must leave no Compaction entry"
    );
    assert!(
        !handle.snapshot().iter().any(|event| matches!(
            event,
            AgentEvent::Notice(text) if text.starts_with("compacted:")
        )),
        "a release-dropped queued Compact must leave no projection"
    );
}

#[tokio::test]
async fn steer_release_without_queued_prompt_wait_for_input_returns_to_idle() {
    // Contract 6 (WaitForInput side): the same release parks the session at
    // Idle — ready for a future prompt, never Finished — because a
    // WaitForInput session is long-lived by policy.
    let temp = tempfile::tempdir().unwrap();
    let (agent, entered, _release) = recovering_agent(
        vec![
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
        "steer-wfi-noqueue".into(),
        IdlePolicy::WaitForInput,
    );
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(Some("initial".into()));
    entered.notified().await;
    handle.cancel();

    loop {
        match live.recv().await.unwrap() {
            AgentEvent::Notice(text) if text == "turn cancelled" => break,
            _ => {}
        }
    }
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;
    assert!(!matches!(*status.borrow(), SessionStatus::Finished(_)));

    // The session is alive and waiting: a follow-up prompt still works.
    handle.prompt("follow up");
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "follow-up answer" => break,
            AgentEvent::Error(_) => panic!("follow-up turn failed"),
            _ => {}
        }
    }
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;
    assert!(!matches!(*status.borrow(), SessionStatus::Finished(_)));
    drop(handle);
    drop(task);
}

#[tokio::test]
async fn steer_btw_wait_for_input_consumes_queued_prompts_then_returns_to_idle() {
    // Contract 7: a WaitForInput session ("btw") processes queued prompts
    // after a release exactly like FinishWhenIdle, but returns to Idle
    // afterwards — it never finishes naturally.
    let temp = tempfile::tempdir().unwrap();
    let (agent, entered, _release) = recovering_agent(
        vec![
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
        "steer-btw".into(),
        IdlePolicy::WaitForInput,
    );
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(Some("initial".into()));
    entered.notified().await;
    handle.prompt("queued while busy");
    handle.cancel();

    loop {
        match live.recv().await.unwrap() {
            AgentEvent::Notice(text) if text == "turn cancelled" => break,
            _ => {}
        }
    }
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "queued answer" => break,
            _ => {}
        }
    }
    // Processed, back at Idle — not finished.
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;
    assert!(!matches!(*status.borrow(), SessionStatus::Finished(_)));

    // The session is still alive for a plain follow-up prompt.
    handle.prompt("follow up");
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "follow-up answer" => break,
            AgentEvent::Error(_) => panic!("follow-up turn failed"),
            _ => {}
        }
    }
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;
    assert!(!matches!(*status.borrow(), SessionStatus::Finished(_)));
    drop(handle);
    drop(task);
}

#[tokio::test]
async fn steer_hard_abort_still_drops_in_flight_operation_immediately() {
    // Contract 8: the DELETE / task-panel hard termination is the runner
    // task abort — it must keep working unchanged: the in-flight operation
    // is dropped immediately, its side effects never run, and the handle
    // stops accepting commands.
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
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "steer-hard-abort".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let mut task = runner.start(Some("start".into()));
    entered.notified().await;
    task.abort();
    dropped.notified().await;
    release.notify_one();
    tokio::task::yield_now().await;
    assert_eq!(side_effects.load(Ordering::SeqCst), 0);
    // The handle is closed: further prompts are silently dropped.
    let before = handle.snapshot();
    handle.prompt("too late");
    assert_eq!(handle.snapshot(), before);
    drop(task);
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Steering v2 — step 3: idle-select blind spots (B1/B2a/B3/B4)
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

#[tokio::test]
async fn steer_cancel_while_idle_waiting_on_blocking_background_finalizes_cancelled_immediately() {
    // B1: a FinishWhenIdle session parked at the idle select waiting for a
    // blocking background task must finalize
    // Cancelled IMMEDIATELY on a cancel — it must not keep sleeping out the
    // the idle wait. The background task is NOT cancelled (it stays in
    // the shared registry), and the session ends through the explicit Cancel
    // path, rather than remaining indefinitely blocked on the background task.
    let temp = tempfile::tempdir().unwrap();
    let workspace = crate::workspace::Workspace::new(temp.path()).unwrap();
    let (tools, background) = crate::tools::builtins(workspace, None, false, None);
    let agent = Agent::new(
        Box::new(ScriptedAssistantModel {
            replies: vec![
                AssistantMessage {
                    content: None,
                    tool_calls: vec![background_bash_call("sleep 30", false)],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("build started, waiting".into()),
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
        "steer-bg-cancel".into(),
        IdlePolicy::FinishWhenIdle,
    );
    // The explicit Cancel must finalize immediately; without it, the session
    // would remain indefinitely blocked on the unfinished background task.
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(Some("run build".into()));

    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "build started, waiting" => break,
            AgentEvent::Error(text) => panic!("turn failed: {text}"),
            _ => {}
        }
    }
    // The session parks at Idle: the unfinished blocking background task
    // prevents natural finalization.
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;

    handle.cancel();

    // The explicit Cancel finalizes Cancelled immediately instead of leaving
    // the session indefinitely blocked on the unfinished background task.
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))),
    )
    .await
    .expect(
        "cancel at the idle select must finalize Cancelled immediately, without waiting for task completion",
    );
    assert_eq!(result, SessionStatus::Finished(SessionResult::Cancelled));
    task.join().await.unwrap();

    // The background task was NOT cancelled: it is still registered in the
    // shared registry after the subagent finalized.
    let tasks = background.running();
    assert_eq!(tasks.len(), 1, "task must stay registered: {tasks:?}");
    assert_eq!(tasks[0].kind, "bash");
    // Explicitly cancel to avoid leaking the sleep process past the test.
    background.cancel(tasks[0].id);
    assert!(background.running().is_empty());
}

#[tokio::test]
async fn steer_cancel_then_prompt_at_idle_with_blocking_background_runs_queued_turn_then_bg_completion_round()
 {
    // B1 variant: cancel + prompt in the same tick at the idle select while
    // a blocking background task runs. The release is recomputed as
    // ReleasedWithPrompts (merge_steering classifies from the final queue
    // state, and the idle-select site only finalizes the no-prompts case),
    // so the queued prompt opens a fresh turn that completes naturally; the
    // later background completion then injects the empty synthetic prompt
    // and drives a final round — the session finalizes Completed through
    // that round, rather than remaining indefinitely blocked.
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
                    content: Some("queued follow-up answer".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("bg done, final".into()),
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
        "steer-bg-cancel-prompt".into(),
        IdlePolicy::FinishWhenIdle,
    );
    // Without the explicit Cancel plus queued prompt, the session would
    // remain indefinitely blocked on the unfinished background task instead
    // of running the queued turn.
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(Some("run build".into()));

    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "build started, waiting" => break,
            AgentEvent::Error(text) => panic!("turn failed: {text}"),
            _ => {}
        }
    }
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;

    // cancel + prompt back-to-back (single-threaded test runtime: both land
    // in the channel before the runner polls again): the release classifies
    // as ReleasedWithPrompts and the queued prompt runs as a fresh turn.
    handle.cancel();
    handle.prompt("follow up");
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::UserPrompt(text) if text == "follow up" => break,
            AgentEvent::Error(text) => panic!("turn failed: {text}"),
            _ => {}
        }
    }
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "queued follow-up answer" => break,
            AgentEvent::Error(text) => panic!("turn failed: {text}"),
            _ => {}
        }
    }

    // The blocking background task completes: the runner commits the
    // completion and injects the empty synthetic prompt, driving a final
    // model round whose answer finalizes the session.
    sender
        .lock()
        .unwrap()
        .as_ref()
        .expect("agent must wire the bash completion sender")
        .send(AgentEvent::BackgroundCompleted {
            id: 9,
            output: "build ok".into(),
            label: Some("cargo build".into()),
            started_at_ms: None,
            duration_ms: None,
            exit_code: None,
            signal: None,
            status: None,
            kind: None,
        })
        .unwrap();
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "bg done, final" => break,
            AgentEvent::Error(text) => panic!("follow-up turn failed: {text}"),
            _ => {}
        }
    }
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert_eq!(
        result,
        SessionStatus::Finished(SessionResult::Completed(Some("bg done, final".into())))
    );
    task.join().await.unwrap();
}

#[tokio::test]
async fn steer_cancel_then_prompt_at_idle_select_wait_for_input_opens_new_turn_and_double_cancel_is_noop()
 {
    // B2a + B3: a WaitForInput session parked at the idle select receives a
    // Cancel (release) — it returns to Idle (never Finished) — then an
    // immediate follow-up prompt opens a fresh turn that completes normally
    // (pins the merge_steering recompute at the idle-select command intake
    // and the release-returns-to-Idle path under WaitForInput). B3 rides
    // along: a second Cancel sent while Idle is a no-op — the session state
    // stays Idle, unchanged.
    let temp = tempfile::tempdir().unwrap();
    let (agent, _entered, _release) = recovering_agent(
        vec![
            Ok(AssistantMessage {
                content: Some("first answer".into()),
                tool_calls: Vec::new(),
                reasoning: None,
            }),
            Ok(AssistantMessage {
                content: Some("follow-up answer".into()),
                tool_calls: Vec::new(),
                reasoning: None,
            }),
        ],
        false, // no blocking: the first turn completes naturally
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "steer-wfi-idle-cancel".into(),
        IdlePolicy::WaitForInput,
    );
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(Some("initial".into()));

    // First turn completes naturally -> the runner parks at the idle select.
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "first answer" => break,
            AgentEvent::Error(text) => panic!("turn failed: {text}"),
            _ => {}
        }
    }
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;

    // B3 first: a Cancel at the idle select is a release that returns to
    // Idle; a second Cancel right behind it is a no-op — the session state
    // stays Idle, never Finished.
    handle.cancel();
    handle.cancel();
    tokio::task::yield_now().await;
    tokio::task::yield_now().await; // let the runner consume both cancels
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;
    assert!(
        !matches!(*status.borrow(), SessionStatus::Finished(_)),
        "double cancel must not change the WaitForInput session state"
    );

    // B2a: the prompt that follows the cancels opens a fresh turn that
    // completes normally.
    handle.prompt("follow up");
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "follow-up answer" => break,
            AgentEvent::Error(text) => panic!("follow-up turn failed: {text}"),
            _ => {}
        }
    }
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Idle)).await;
    assert!(!matches!(*status.borrow(), SessionStatus::Finished(_)));
    drop(handle);
    drop(task);
}

#[tokio::test]
async fn steer_fifo_queued_compact_prompt_cancel_runs_compact_before_prompt() {
    // B4: [Compact, prompt, cancel] queued while a round is in flight. The
    // cancel releases the round; the pending queue keeps its FIFO order, so
    // the outer loop runs the Compact FIRST (the compact-first branch only
    // pops when Compact is at the front) and the queued prompt SECOND — the
    // Compaction entry lands before the prompt's User entry, and the prompt
    // turn then completes naturally.
    let temp = tempfile::tempdir().unwrap();
    let (mut agent, entered, _release) = recovering_agent(
        vec![
            Ok(AssistantMessage {
                content: Some("interrupted".into()),
                tool_calls: Vec::new(),
                reasoning: None,
            }),
            Ok(AssistantMessage {
                content: Some("The compact summary of the earlier conversation preserves the earlier question and the assistant's earlier reply, plus the context needed to continue with the current work.".into()),
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
    history_for_compaction(&mut agent);
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "steer-fifo".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(Some("initial".into()));

    entered.notified().await; // round in flight
    handle.compact();
    handle.prompt("queued prompt");
    handle.cancel();

    // The release preempts the round ("turn cancelled"), then the queue is
    // drained in FIFO order: compact first (compacted: projection Notice),
    // prompt second (UserPrompt).
    let mut order: Vec<String> = Vec::new();
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::Notice(text)
                if text == "turn cancelled" || text.starts_with("compacted:") =>
            {
                order.push(text);
            }
            AgentEvent::UserPrompt(text) if text == "queued prompt" => {
                order.push("user:queued prompt".into());
                break;
            }
            _ => {}
        }
    }
    let cancelled = order
        .iter()
        .position(|text| text == "turn cancelled")
        .expect("the release must emit 'turn cancelled'");
    let compact = order
        .iter()
        .position(|text| text.starts_with("compacted:"))
        .expect("the queued Compact must run before the prompt (FIFO)");
    assert!(
        cancelled < compact,
        "release first, then compact: {order:?}"
    );
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "queued answer" => break,
            AgentEvent::Error(text) => panic!("queued turn failed: {text}"),
            _ => {}
        }
    }
    let result = wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert_eq!(
        result,
        SessionStatus::Finished(SessionResult::Completed(Some("queued answer".into())))
    );
    task.join().await.unwrap();

    // Durable FIFO proof: in the session file the Compaction entry precedes
    // the queued prompt's User entry.
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "steer-fifo")
        .await
        .unwrap();
    let compact_idx = loaded
        .entries
        .iter()
        .position(|entry| matches!(entry, SessionEntry::Compaction { .. }))
        .expect("compaction must run before the prompt");
    let prompt_idx = loaded
        .entries
        .iter()
        .position(|entry| {
            matches!(
                entry,
                SessionEntry::Message {
                    message: Message::User { content, .. }
                } if content == "queued prompt"
            )
        })
        .expect("queued prompt must run after the compaction");
    assert!(
        compact_idx < prompt_idx,
        "FIFO: Compaction entry ({compact_idx}) must precede the queued prompt ({prompt_idx})"
    );
    assert!(loaded.entries.iter().any(|entry| matches!(
        entry,
        SessionEntry::Compaction { summary, .. } if summary == "The compact summary of the earlier conversation preserves the earlier question and the assistant's earlier reply, plus the context needed to continue with the current work."
    )));
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Session goals: commands, tool CAS, persistence, isolation
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

async fn wait_for_goal(
    handle: &SessionHandle,
    expected: impl Fn(Option<&crate::agent::GoalSnapshot>) -> bool,
) {
    for _ in 0..400 {
        if expected(handle.goal().as_ref()) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("goal condition not met in time");
}

async fn wait_for_log_event(handle: &SessionHandle, expected: impl Fn(&AgentEvent) -> bool) {
    for _ in 0..400 {
        if handle.snapshot().iter().any(&expected) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("goal event not found in time");
}

#[test]
fn goal_command_reports_closed_channel_and_live_send() {
    // Live channel: goal_command returns true — the command was accepted
    // for queuing (the runner applies it asynchronously).
    let (live_handle, _emitter, _receiver) = session_test_channel();
    assert!(live_handle.goal_command(GoalCommand::Create {
        objective: "ship it".into(),
        success_criteria: vec![],
    }));

    // Closed channel (receiver dropped) with status NOT Finished: must
    // return false — the closed-channel fallback is reachable without the
    // Finished status precheck (HTTP POST relies on this exact path for
    // its 409 when the runner task has exited).
    let (closed_handle, _emitter, receiver) = session_test_channel();
    drop(receiver);
    assert_eq!(
        &*closed_handle.status().borrow(),
        &SessionStatus::Idle,
        "precondition: status must be Idle, not Finished"
    );
    assert!(!closed_handle.goal_command(GoalCommand::Create {
        objective: "nope".into(),
        success_criteria: vec![],
    }));
    assert!(!closed_handle.goal_command(GoalCommand::Action(crate::agent::GoalAction::Clear)));
}

#[tokio::test]
async fn goal_commands_create_actions_and_clear_persist_and_fan_out() {
    let temp = tempfile::tempdir().unwrap();
    let agent = Agent::new(
        Box::new(ScriptedAssistantModel {
            replies: VecDeque::new(),
        }),
        // KeepAliveTool holds the background sender open so the idle
        // WaitForInput runner does not finalize Closed between commands.
        vec![Box::new(KeepAliveTool { sender: None })],
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "goal-sess".into(),
        IdlePolicy::WaitForInput,
    );
    let _task = runner.start(None);
    assert!(handle.goal().is_none());

    // Human create: revision 1, active.
    handle.goal_command(GoalCommand::Create {
        objective: "ship the goal system".into(),
        success_criteria: vec!["tests pass".into()],
    });
    wait_for_goal(&handle, |g| g.is_some()).await;
    let goal = handle.goal().unwrap();
    assert_eq!(goal.revision, 1);
    assert_eq!(goal.status, crate::agent::GoalStatus::Active);

    // Create while active is rejected: an Error event, goal unchanged, no
    // extra persisted entry.
    handle.goal_command(GoalCommand::Create {
        objective: "second".into(),
        success_criteria: vec![],
    });
    wait_for_log_event(&handle, |event| {
        matches!(event, AgentEvent::Error(text) if text.contains("cannot create a new goal"))
    })
    .await;
    assert_eq!(handle.goal().unwrap().revision, 1);

    // pause → resume → clear, each a durable snapshot with a bumped rev.
    handle.goal_command(GoalCommand::Action(crate::agent::GoalAction::Pause));
    wait_for_goal(&handle, |g| {
        g.map(|g| g.status) == Some(crate::agent::GoalStatus::Paused)
    })
    .await;
    handle.goal_command(GoalCommand::Action(crate::agent::GoalAction::Resume));
    wait_for_goal(&handle, |g| {
        g.map(|g| g.status) == Some(crate::agent::GoalStatus::Active)
    })
    .await;
    handle.goal_command(GoalCommand::Action(crate::agent::GoalAction::Clear));
    wait_for_goal(&handle, |g| g.is_none()).await;

    // Persistence: exactly create + pause + resume + clear, all complete
    // snapshots; the fold of the persisted log is None (cleared).
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "goal-sess")
        .await
        .unwrap();
    let updates: Vec<&SessionEntry> = loaded
        .entries
        .iter()
        .filter(|entry| matches!(entry, SessionEntry::GoalUpdated { .. }))
        .collect();
    assert_eq!(updates.len(), 4, "rejected create appends nothing");
    assert!(matches!(
        updates[0],
        SessionEntry::GoalUpdated { goal: Some(_) }
    ));
    assert!(matches!(
        updates[3],
        SessionEntry::GoalUpdated { goal: None }
    ));
    let folded = loaded
        .entries
        .iter()
        .rev()
        .find(|entry| matches!(entry, SessionEntry::GoalUpdated { .. }));
    assert!(matches!(
        folded,
        Some(SessionEntry::GoalUpdated { goal: None })
    ));
    // The runner's own event log carries the GoalUpdated fanout.
    assert!(handle.snapshot().iter().any(|event| {
        matches!(event, AgentEvent::GoalUpdated { goal: Some(g) } if g.revision == 2)
    }));
}

#[tokio::test]
async fn update_goal_tool_cas_updates_and_completes_with_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let goal = crate::agent::create_goal(None, "build it".into(), vec![]).unwrap();
    let id = goal.id.clone();
    let rev1 = goal.revision;
    // One turn, three tool rounds: progress (rev+1), complete with
    // evidence (rev+1), then a STALE revision call that must be rejected.
    let model = ScriptedAssistantModel {
        replies: VecDeque::from(vec![
            AssistantMessage {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "update_goal".into(),
                    arguments: serde_json::json!({
                        "id": id, "revision": rev1, "action": "progress", "progress": "core done"
                    })
                    .to_string(),
                }],
                reasoning: None,
            },
            AssistantMessage {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "c2".into(),
                    name: "update_goal".into(),
                    arguments: serde_json::json!({
                        "id": id, "revision": rev1 + 1, "action": "complete",
                        "evidence": ["unverified: analysis passed"]
                    })
                    .to_string(),
                }],
                reasoning: None,
            },
            AssistantMessage {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "c3".into(),
                    name: "update_goal".into(),
                    arguments: serde_json::json!({
                        "id": id, "revision": rev1, "action": "pause"
                    })
                    .to_string(),
                }],
                reasoning: None,
            },
            AssistantMessage {
                content: Some("done".into()),
                tool_calls: vec![],
                reasoning: None,
            },
        ]),
    };
    let mut agent = Agent::new(
        Box::new(model),
        vec![Box::new(KeepAliveTool { sender: None })],
    );
    agent.restore_history(vec![SessionEntry::GoalUpdated {
        goal: Some(goal.clone()),
    }]);
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "goal-tool".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let _task = runner.start(Some("work on the goal".into()));
    let mut status = handle.status();
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;

    // Completed, evidence kept, progress kept; two CAS-successful bumps.
    let final_goal = handle.goal().unwrap();
    assert_eq!(final_goal.status, crate::agent::GoalStatus::Completed);
    assert_eq!(final_goal.revision, rev1 + 2);
    assert_eq!(final_goal.progress, "core done");
    assert_eq!(final_goal.evidence, vec!["unverified: analysis passed"]);

    // Persisted: exactly two GoalUpdated entries (the stale call appended
    // nothing); the stale tool result is an error Tool entry.
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "goal-tool")
        .await
        .unwrap();
    let updates: Vec<&SessionEntry> = loaded
        .entries
        .iter()
        .filter(|entry| matches!(entry, SessionEntry::GoalUpdated { .. }))
        .collect();
    assert_eq!(updates.len(), 2);
    assert!(loaded.entries.iter().any(|entry| matches!(
        entry,
        SessionEntry::Message {
            message: Message::Tool { name, is_error: true, content, .. }
        } if name == "update_goal" && content.contains("revision mismatch")
    )));
}

#[tokio::test]
async fn goal_tools_are_isolated_per_session_fresh_session_has_no_goal() {
    // A subagent's runner is a separate Agent/runner pair: the default
    // (usually no goal) state is visible to it, and the model's
    // update_goal cannot create one — it errors instead of inventing one.
    let temp = tempfile::tempdir().unwrap();
    let model = ScriptedAssistantModel {
        replies: VecDeque::from(vec![
            AssistantMessage {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "update_goal".into(),
                    arguments: r#"{"id":"goal-x","revision":1,"action":"pause"}"#.into(),
                }],
                reasoning: None,
            },
            AssistantMessage {
                content: Some("done".into()),
                tool_calls: vec![],
                reasoning: None,
            },
        ]),
    };
    let agent = Agent::new(
        Box::new(model),
        vec![Box::new(KeepAliveTool { sender: None })],
    );
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "goal-empty".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let _task = runner.start(Some("work".into()));
    let mut status = handle.status();
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;
    assert!(handle.goal().is_none());
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "goal-empty")
        .await
        .unwrap();
    assert!(
        !loaded
            .entries
            .iter()
            .any(|entry| matches!(entry, SessionEntry::GoalUpdated { .. })),
        "update_goal must not create a goal"
    );
    assert!(loaded.entries.iter().any(|entry| matches!(
        entry,
        SessionEntry::Message {
            message: Message::Tool { name, is_error: true, content, .. }
        } if name == "update_goal" && content.contains("no goal is set")
    )));
}

/// Model that issues `get_goal`, then reads the id AND revision from the
/// get_goal TOOL OUTPUT in the next context and issues `update_goal` with
/// them — the test's CAS round trip never borrows either from the fixture.
struct GoalRoundTripModel {
    calls: usize,
    /// Revision parsed from the get_goal tool output (shared for asserts).
    seen_revision: Arc<Mutex<Option<u64>>>,
    /// Id parsed from the get_goal tool output (shared for asserts).
    seen_id: Arc<Mutex<Option<String>>>,
    /// Full success_criteria array observed in the get_goal output.
    seen_criteria: Arc<Mutex<Option<Vec<String>>>>,
}

#[async_trait]
impl Model for GoalRoundTripModel {
    async fn complete(
        &mut self,
        messages: &[Message],
        _: &[ToolSpec],
        _: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        self.calls += 1;
        if self.calls == 1 {
            return Ok((
                AssistantMessage {
                    content: None,
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "get_goal".into(),
                        arguments: "{}".into(),
                    }],
                    reasoning: None,
                },
                None,
            ));
        }
        if self.calls == 2 {
            // Parse the FULL snapshot out of the committed get_goal result:
            // the id and revision used for the CAS must come from here,
            // never from the fixture.
            let mut snapshot: Option<serde_json::Value> = None;
            for message in messages {
                if let Message::Tool {
                    name,
                    content,
                    is_error: false,
                    ..
                } = message
                    && name == "get_goal"
                {
                    snapshot = serde_json::from_str(content).ok();
                }
            }
            let snapshot = snapshot.expect("get_goal must return a JSON snapshot");
            for key in [
                "id",
                "revision",
                "objective",
                "success_criteria",
                "status",
                "progress",
                "evidence",
                "blocked_reason",
            ] {
                assert!(
                    snapshot.get(key).is_some(),
                    "get_goal snapshot missing `{key}`: {snapshot}"
                );
            }
            let id = snapshot["id"].as_str().expect("id must be a string");
            let revision = snapshot["revision"]
                .as_u64()
                .expect("revision must be an integer");
            let criteria = snapshot["success_criteria"]
                .as_array()
                .expect("success_criteria must be the full array")
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>();
            *self.seen_revision.lock().unwrap() = Some(revision);
            *self.seen_id.lock().unwrap() = Some(id.to_owned());
            *self.seen_criteria.lock().unwrap() = Some(criteria);
            assert_eq!(snapshot["objective"], "build it");
            assert_eq!(snapshot["status"], "active");
            return Ok((
                AssistantMessage {
                    content: None,
                    tool_calls: vec![ToolCall {
                        id: "c2".into(),
                        name: "update_goal".into(),
                        arguments: serde_json::json!({
                            "id": id, "revision": revision, "action": "progress",
                            "progress": "round trip"
                        })
                        .to_string(),
                    }],
                    reasoning: None,
                },
                None,
            ));
        }
        Ok((
            AssistantMessage {
                content: Some("done".into()),
                tool_calls: vec![],
                reasoning: None,
            },
            None,
        ))
    }
}

#[tokio::test]
async fn get_goal_returns_full_snapshot_and_update_reads_revision_from_it() {
    // The model genuinely reads the revision from get_goal's JSON output
    // before updating — proving the tool output is a complete, machine-
    // usable snapshot (not the short provider projection).
    let temp = tempfile::tempdir().unwrap();
    let goal = crate::agent::create_goal(
        None,
        "build it".into(),
        vec!["tests pass".into(), "docs updated".into()],
    )
    .unwrap();
    let seen_revision = Arc::new(Mutex::new(None));
    let seen_id = Arc::new(Mutex::new(None));
    let seen_criteria = Arc::new(Mutex::new(None));
    let model = GoalRoundTripModel {
        calls: 0,
        seen_revision: seen_revision.clone(),
        seen_id: seen_id.clone(),
        seen_criteria: seen_criteria.clone(),
    };
    let mut agent = Agent::new(
        Box::new(model),
        vec![Box::new(KeepAliveTool { sender: None })],
    );
    agent.restore_history(vec![SessionEntry::GoalUpdated {
        goal: Some(goal.clone()),
    }]);
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "goal-roundtrip".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let _task = runner.start(Some("work on the goal".into()));
    let mut status = handle.status();
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;

    // The id AND revision the model read both came from get_goal output and
    // matched the fixture's; the full criteria array was visible.
    assert_eq!(
        *seen_revision.lock().unwrap(),
        Some(goal.revision),
        "revision must come from get_goal output"
    );
    assert_eq!(
        *seen_id.lock().unwrap(),
        Some(goal.id.clone()),
        "id must come from get_goal output"
    );
    assert_eq!(
        *seen_criteria.lock().unwrap(),
        Some(vec!["tests pass".to_owned(), "docs updated".to_owned()])
    );
    // The CAS succeeded with the snapshot-derived id+revision: progress
    // applied, revision bumped once, the same goal id untouched.
    let final_goal = handle.goal().unwrap();
    assert_eq!(final_goal.revision, goal.revision + 1);
    assert_eq!(final_goal.id, goal.id);
    assert_eq!(final_goal.progress, "round trip");
}

#[tokio::test]
async fn update_goal_rejects_wrong_typed_criteria_and_evidence() {
    // `success_criteria` / `evidence` present but not a string array: a
    // plain tool error, never a silent filter.
    let temp = tempfile::tempdir().unwrap();
    let goal = crate::agent::create_goal(None, "build it".into(), vec![]).unwrap();
    let id = goal.id.clone();
    let rev = goal.revision;
    let model = ScriptedAssistantModel {
        replies: VecDeque::from(vec![
            AssistantMessage {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "update_goal".into(),
                    arguments: serde_json::json!({
                        "id": id, "revision": rev, "action": "progress", "progress": "p",
                        "success_criteria": ["ok", 42]
                    })
                    .to_string(),
                }],
                reasoning: None,
            },
            AssistantMessage {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "c2".into(),
                    name: "update_goal".into(),
                    arguments: serde_json::json!({
                        "id": id, "revision": rev, "action": "progress", "progress": "q",
                        "evidence": "not-an-array"
                    })
                    .to_string(),
                }],
                reasoning: None,
            },
            AssistantMessage {
                content: Some("done".into()),
                tool_calls: vec![],
                reasoning: None,
            },
        ]),
    };
    let mut agent = Agent::new(
        Box::new(model),
        vec![Box::new(KeepAliveTool { sender: None })],
    );
    agent.restore_history(vec![SessionEntry::GoalUpdated {
        goal: Some(goal.clone()),
    }]);
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "goal-strict".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let _task = runner.start(Some("work".into()));
    let mut status = handle.status();
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;

    // Both calls failed with plain errors; nothing was committed.
    let final_goal = handle.goal().unwrap();
    assert_eq!(final_goal.revision, rev, "no successful transition");
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "goal-strict")
        .await
        .unwrap();
    let errors: Vec<&str> = loaded
        .entries
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::Message {
                message:
                    Message::Tool {
                        name,
                        content,
                        is_error: true,
                        ..
                    },
            } if name == "update_goal" => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 2, "both wrong-typed calls must error");
    assert!(
        errors
            .iter()
            .any(|e| e.contains("success_criteria") && e.contains("array"))
    );
    assert!(
        errors
            .iter()
            .any(|e| e.contains("evidence") && e.contains("array"))
    );
}

#[tokio::test]
async fn update_goal_rejects_unknown_fields_without_commit() {
    // Misspelled / arbitrary extra keys are rejected BEFORE any transition
    // or commit: a plain model-facing tool error naming the field(s), the
    // revision untouched, and no GoalUpdated entry appended.
    let temp = tempfile::tempdir().unwrap();
    let goal = crate::agent::create_goal(None, "build it".into(), vec![]).unwrap();
    let id = goal.id.clone();
    let rev = goal.revision;
    let model = ScriptedAssistantModel {
        replies: VecDeque::from(vec![
            AssistantMessage {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "update_goal".into(),
                    arguments: serde_json::json!({
                        "id": id, "revision": rev, "action": "progress", "progress": "p",
                        "sucess_criteria": ["typo"]
                    })
                    .to_string(),
                }],
                reasoning: None,
            },
            AssistantMessage {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "c2".into(),
                    name: "update_goal".into(),
                    arguments: serde_json::json!({
                        "id": id, "revision": rev, "action": "pause",
                        "bogus": {"nested": true}, "extra": 1
                    })
                    .to_string(),
                }],
                reasoning: None,
            },
            AssistantMessage {
                content: Some("done".into()),
                tool_calls: vec![],
                reasoning: None,
            },
        ]),
    };
    let mut agent = Agent::new(
        Box::new(model),
        vec![Box::new(KeepAliveTool { sender: None })],
    );
    agent.restore_history(vec![SessionEntry::GoalUpdated {
        goal: Some(goal.clone()),
    }]);
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "goal-unknown".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let _task = runner.start(Some("work".into()));
    let mut status = handle.status();
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;

    // No successful transition: revision untouched, status still active,
    // no progress applied.
    let final_goal = handle.goal().unwrap();
    assert_eq!(
        final_goal.revision, rev,
        "unknown fields must never bump the revision"
    );
    assert_eq!(final_goal.status, crate::agent::GoalStatus::Active);
    assert_eq!(final_goal.progress, "", "no progress applied");
    // Persisted: no GoalUpdated entry; both calls errored naming the exact
    // unknown field(s).
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "goal-unknown")
        .await
        .unwrap();
    let updates = loaded
        .entries
        .iter()
        .filter(|entry| matches!(entry, SessionEntry::GoalUpdated { .. }))
        .count();
    assert_eq!(
        updates, 0,
        "unknown-field rejections must append no GoalUpdated"
    );
    let errors: Vec<&str> = loaded
        .entries
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::Message {
                message:
                    Message::Tool {
                        name,
                        content,
                        is_error: true,
                        ..
                    },
            } if name == "update_goal" => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 2, "both unknown-field calls must error");
    assert!(
        errors.iter().any(|e| e.contains("sucess_criteria")),
        "error must name the misspelled field: {errors:?}"
    );
    assert!(
        errors
            .iter()
            .any(|e| e.contains("`bogus`") && e.contains("`extra`")),
        "error must name all extra fields: {errors:?}"
    );
}

#[tokio::test]
async fn get_goal_rejects_unknown_arguments_and_non_object() {
    // get_goal is a read: only the empty JSON object is accepted. Any
    // extra key (or a non-object payload) is a plain tool error naming the
    // field — never silently ignored.
    let temp = tempfile::tempdir().unwrap();
    let goal = crate::agent::create_goal(None, "build it".into(), vec![]).unwrap();
    let model = ScriptedAssistantModel {
        replies: VecDeque::from(vec![
            AssistantMessage {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "get_goal".into(),
                    arguments: r#"{"extra": 1}"#.into(),
                }],
                reasoning: None,
            },
            AssistantMessage {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "c2".into(),
                    name: "get_goal".into(),
                    arguments: r#""not-an-object""#.into(),
                }],
                reasoning: None,
            },
            AssistantMessage {
                content: Some("done".into()),
                tool_calls: vec![],
                reasoning: None,
            },
        ]),
    };
    let mut agent = Agent::new(
        Box::new(model),
        vec![Box::new(KeepAliveTool { sender: None })],
    );
    agent.restore_history(vec![SessionEntry::GoalUpdated {
        goal: Some(goal.clone()),
    }]);
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        temp.path().into(),
        "goal-get-strict".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let _task = runner.start(Some("work".into()));
    let mut status = handle.status();
    wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await;

    let loaded = SessionStore::Jsonl
        .load(temp.path(), "goal-get-strict")
        .await
        .unwrap();
    let errors: Vec<&str> = loaded
        .entries
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::Message {
                message:
                    Message::Tool {
                        name,
                        content,
                        is_error: true,
                        ..
                    },
            } if name == "get_goal" => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 2, "both get_goal calls must error");
    assert!(
        errors.iter().any(|e| e.contains("`extra`")),
        "error must name the unknown get_goal field: {errors:?}"
    );
    assert!(
        errors.iter().any(|e| e.contains("must be a JSON object")),
        "non-object get_goal arguments must error: {errors:?}"
    );
    // get_goal never mutates: the goal snapshot is untouched.
    assert_eq!(handle.goal().unwrap().revision, goal.revision);
    assert_eq!(handle.goal().unwrap().id, goal.id);
}

struct SlowTool {
    millis: u64,
}

#[async_trait]
impl Tool for SlowTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "slow_test".into(),
            description: "test only".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }
    async fn execute(&self, _: Value) -> Result<ToolOutput, String> {
        tokio::time::sleep(std::time::Duration::from_millis(self.millis)).await;
        Ok(ToolOutput::text("slow done"))
    }
}

/// Wait for the poll-guard termination notice; panic on a failed turn.
async fn wait_for_termination_notice(live: &mut tokio::sync::broadcast::Receiver<AgentEvent>) {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            match live.recv().await.unwrap() {
                AgentEvent::Notice(text) if text == POLL_GUARD_TERMINATION_NOTICE => break,
                AgentEvent::Error(text) => panic!("turn failed: {text}"),
                _ => {}
            }
        }
    })
    .await
    .expect("poll-guard termination notice timed out");
}

fn poll_call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "get_background_tasks".into(),
        arguments: "{}".into(),
    }
}

fn tool_entries(entries: &[SessionEntry]) -> Vec<&Message> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::Message { message } => match message {
                Message::Tool { .. } => Some(message),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn poll_guard_finish_when_idle_waits_for_owned_completion_and_resets() {
    // The threshold ends only the current turn. FinishWhenIdle must keep the
    // session alive for its owned non-detached task, then the completion
    // safe-point/follow-up starts a fresh turn with a reset guard.
    let temp = tempfile::tempdir().unwrap();
    let workspace = crate::workspace::Workspace::new(temp.path()).unwrap();
    let (_main_tools, background) = crate::tools::builtins(workspace.clone(), None, false, None);
    let tools = crate::tools::builtins_with_background(
        workspace,
        background.clone(),
        None,
        false,
        true,
        None,
    );
    let agent = Agent::new(
        Box::new(ScriptedAssistantModel {
            replies: vec![
                AssistantMessage {
                    content: None,
                    tool_calls: vec![
                        ToolCall {
                            id: "c0".into(),
                            name: "bash".into(),
                            arguments: r#"{"command":"sleep 1; echo done","background":true}"#
                                .into(),
                        },
                        poll_call("c1"),
                        poll_call("c2"),
                        poll_call("c3"),
                    ],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("final after completion".into()),
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
        "poll-guard-finish-when-idle".into(),
        IdlePolicy::FinishWhenIdle,
    );
    let (_, mut live, mut status) = handle.attach();
    let task = runner.start(Some("wait for build".into()));

    wait_for_termination_notice(&mut live).await;
    assert!(matches!(*status.borrow(), SessionStatus::Idle));
    assert!(background.running().iter().any(|task| task.id == 1));

    let answer = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match live.recv().await.unwrap() {
                AgentEvent::AssistantText(text) if text == "final after completion" => break,
                AgentEvent::Error(text) => panic!("follow-up turn failed: {text}"),
                _ => {}
            }
        }
        wait_for_status(&mut status, |s| matches!(s, SessionStatus::Finished(_))).await
    })
    .await
    .expect("owned completion must trigger the FinishWhenIdle follow-up");
    assert_eq!(
        answer,
        SessionStatus::Finished(SessionResult::Completed(Some(
            "final after completion".into()
        )))
    );
    task.join().await.unwrap();

    let loaded = SessionStore::Jsonl
        .load(temp.path(), "poll-guard-finish-when-idle")
        .await
        .unwrap();
    let serialized = serde_json::to_string(&loaded.entries).unwrap();
    assert!(!serialized.contains(crate::agent::POLL_GUARD_SENTINEL));
    assert!(
        loaded
            .entries
            .iter()
            .any(|entry| matches!(entry, SessionEntry::BackgroundCompletion { id: 1, .. }))
    );
}

#[tokio::test]
async fn poll_guard_runner_repeated_empty_polls_never_terminate_the_turn() {
    // Regression: a turn that polls `get_background_tasks` repeatedly with
    // NO running tasks must never latch the poll guard — the turn ends
    // with the real final answer, not the termination sentinel (which left
    // the runner's last_answer None).
    let temp = tempfile::tempdir().unwrap();
    let workspace = crate::workspace::Workspace::new(temp.path()).unwrap();
    let (_main_tools, background) = crate::tools::builtins(workspace.clone(), None, false, None);
    let tools =
        crate::tools::builtins_with_background(workspace, background, None, false, true, None);
    let agent = Agent::new(
        Box::new(ScriptedAssistantModel {
            replies: vec![
                AssistantMessage {
                    content: None,
                    tool_calls: vec![
                        poll_call("c1"),
                        poll_call("c2"),
                        poll_call("c3"),
                        poll_call("c4"),
                        poll_call("c5"),
                    ],
                    reasoning: None,
                },
                AssistantMessage {
                    content: None,
                    tool_calls: vec![poll_call("c6"), poll_call("c7"), poll_call("c8")],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("finished".into()),
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
        "poll-guard-empty".into(),
        IdlePolicy::WaitForInput,
    );
    let (_, mut live, _) = handle.attach();
    let task = runner.start(Some("poll a lot".into()));

    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "finished" => break,
            AgentEvent::Notice(text) if text == POLL_GUARD_TERMINATION_NOTICE => {
                panic!("empty polls must never terminate the turn")
            }
            AgentEvent::Error(text) => panic!("turn failed: {text}"),
            _ => {}
        }
    }
    drop(task);

    // Every poll kept its plain "no tasks" ToolResult — no POLL_GUARD_ERROR
    // content, no sentinel anywhere in the durable session file.
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "poll-guard-empty")
        .await
        .unwrap();
    let tools = tool_entries(&loaded.entries);
    assert_eq!(tools.len(), 8, "all eight polls committed");
    for message in tools {
        assert!(matches!(
            message,
            Message::Tool { content, is_error: false, synthetic: false, .. }
                if content == "No background tasks running."
        ));
    }
    assert!(
        !std::fs::read_to_string(temp.path().join(".e-agent/sessions/poll-guard-empty.jsonl"))
            .unwrap()
            .contains(crate::agent::POLL_GUARD_SENTINEL)
    );
}

#[tokio::test]
async fn poll_guard_runner_durable_batch_safe_point_and_next_turn_reset() {
    // Subagent runner: a [bash, poll x3, slow] batch. The real background
    // task is started before all three polls and remains live for them. The
    // third unchanged poll returns the terminal sentinel and latches the guard, but the whole batch is durably
    // committed (including the sibling slow result and the non-synthetic
    // ToolResults). The background completion is committed at the
    // commit_backgrounds safe point BEFORE the termination notice, then a
    // following turn observes an empty snapshot with a reset guard.
    let temp = tempfile::tempdir().unwrap();
    let workspace = crate::workspace::Workspace::new(temp.path()).unwrap();
    let (_main_tools, background) = crate::tools::builtins(workspace.clone(), None, false, None);
    let mut tools =
        crate::tools::builtins_with_background(workspace, background, None, false, true, None);
    tools.push(Box::new(SlowTool { millis: 2500 }));
    let agent = Agent::new(
        Box::new(ScriptedAssistantModel {
            replies: vec![
                AssistantMessage {
                    content: None,
                    tool_calls: vec![
                        ToolCall {
                            id: "c0".into(),
                            name: "bash".into(),
                            arguments: r#"{"command":"sleep 2; echo done","background":true}"#
                                .into(),
                        },
                        poll_call("c1"),
                        poll_call("c2"),
                        poll_call("c3"),
                        ToolCall {
                            id: "c4".into(),
                            name: "slow_test".into(),
                            arguments: "{}".into(),
                        },
                    ],
                    reasoning: None,
                },
                AssistantMessage {
                    content: None,
                    tool_calls: vec![poll_call("c6"), poll_call("c6b"), poll_call("c6c")],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("finished".into()),
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
        "poll-guard-runner".into(),
        IdlePolicy::WaitForInput,
    );
    let (_, mut live, _) = handle.attach();
    let task = runner.start(Some("turn one".into()));

    wait_for_termination_notice(&mut live).await;

    // Safe-point ordering: the completion notice precedes the termination
    // notice in the shared log, and the durable file has no sentinel.
    let snapshot = handle.snapshot();
    let completion_idx = snapshot
        .iter()
        .position(|event| matches!(event, AgentEvent::BackgroundCompletionNotice { id: 1, .. }))
        .expect("completion committed at the safe point");
    let notice_idx = snapshot
        .iter()
        .position(|event| matches!(event, AgentEvent::Notice(text) if text == POLL_GUARD_TERMINATION_NOTICE))
        .expect("termination notice emitted");
    assert!(completion_idx < notice_idx);
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "poll-guard-runner")
        .await
        .unwrap();
    let tools = tool_entries(&loaded.entries);
    assert_eq!(tools.len(), 5, "all first-batch sibling calls committed");
    assert!(matches!(
        tools[0],
        Message::Tool { call_id, content, is_error: false, synthetic: false, .. }
            if call_id == "c0" && content.starts_with("started background task 1:")
    ));
    assert!(matches!(
        tools[1],
        Message::Tool { call_id, content, is_error: false, synthetic: false, .. }
            if call_id == "c1" && content.starts_with("1 background task(s) running:")
    ));
    assert!(matches!(
        tools[2],
        Message::Tool { call_id, content, is_error: true, synthetic: false, .. }
            if call_id == "c2"
                && content.starts_with("1 background task(s) running:")
                && content.contains("#1: ")
                && content.contains(POLL_GUARD_ERROR)
    ));
    assert!(matches!(
        tools[3],
        Message::Tool { call_id, content, is_error: true, synthetic: false, .. }
            if call_id == "c3"
                && content.starts_with("1 background task(s) running:")
                && content.contains("#1: ")
                && content.contains(crate::agent::POLL_GUARD_TERMINATION_NOTICE)
                && !content.contains(crate::agent::POLL_GUARD_SENTINEL)
    ));
    assert!(matches!(
        tools[4],
        Message::Tool { call_id, content, is_error: false, synthetic: false, .. }
            if call_id == "c4" && content == "slow done"
    ));
    assert!(tools.iter().all(|entry| matches!(
        entry,
        Message::Tool {
            synthetic: false,
            ..
        }
    )));
    assert!(loaded.entries.iter().any(|entry| matches!(
        entry,
        SessionEntry::BackgroundCompletion { id: 1, output, .. } if output.contains("done")
    )));
    assert!(
        !std::fs::read_to_string(
            temp.path()
                .join(".e-agent/sessions/poll-guard-runner.jsonl")
        )
        .unwrap()
        .contains(crate::agent::POLL_GUARD_SENTINEL)
    );

    // Next turn (queued prompt): the bash task completed → the snapshot is
    // empty, so the guard never latches and the real answer arrives.
    handle.prompt("turn two");
    loop {
        match live.recv().await.unwrap() {
            AgentEvent::AssistantText(text) if text == "finished" => break,
            AgentEvent::Notice(text) if text == POLL_GUARD_TERMINATION_NOTICE => {
                panic!("empty polls must not terminate turn two")
            }
            AgentEvent::Error(text) => panic!("turn two failed: {text}"),
            _ => {}
        }
    }
    let loaded = SessionStore::Jsonl
        .load(temp.path(), "poll-guard-runner")
        .await
        .unwrap();
    assert_eq!(tool_entries(&loaded.entries).len(), 8);
    for (idx, id) in [(5usize, "c6"), (6, "c6b"), (7, "c6c")] {
        assert!(matches!(
            tool_entries(&loaded.entries)[idx],
            Message::Tool { call_id, content, is_error: false, .. }
                if call_id == id && content == "No background tasks running."
        ));
    }

    drop(handle);
    drop(task);
}
