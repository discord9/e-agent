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
        if matches!(live.recv().await.unwrap(), AgentEvent::UserPrompt(text) if text == "queued while busy")
        {
            break;
        }
    }
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
