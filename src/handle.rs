//! The session seam between agents and their frontends.
//!
//! [`SessionHandle`] is the one contract a UI (today: the TUI; later: a
//! multi-session server or a remote client over WebSocket/HTTP) uses to
//! interact with any agent — main or subagent — without knowing how it is
//! hosted:
//!
//! - `snapshot()` replays what happened so far (for a late-joining view),
//! - `subscribe()` streams new events live,
//! - `send_input()` queues the next user prompt (same semantics as the
//!   main agent's input queue: delivered when the current turn ends),
//! - `cancel()` interrupts the in-flight turn (completed rounds stay).
//!
//! The in-process implementation is [`LiveSession`]; both sides of the
//! steering channel are also exposed separately ([`SessionSink`] /
//! [`SessionSource`]) so the agent-driving thread and the frontend can
//! hold exactly the half each needs.

use std::sync::{Arc, Mutex};

use tokio::sync::{broadcast, mpsc};

use crate::agent::AgentEvent;

/// Broadcast capacity per session. A view follows in real time; a lagged
/// receiver simply misses events (the log still holds the full record).
const SESSION_EVENT_CAPACITY: usize = 256;

/// A steering message from a frontend to a running agent session.
#[derive(Clone, Debug, PartialEq)]
pub enum Steer {
    /// Queue a new user prompt; the agent starts it as a fresh turn once
    /// its current turn ends.
    Prompt(String),
    /// Cancel the in-flight turn (completed rounds stay in history). The
    /// agent then waits for the next prompt.
    Cancel,
}

/// The seam between agents and frontends: one contract for observing and
/// steering any session, local or (later) remote.
pub trait SessionHandle: Send + Sync {
    /// Every event the session has emitted so far.
    fn snapshot(&self) -> Vec<AgentEvent>;
    /// Stream of events from now on (broadcast; lagging misses events).
    fn subscribe(&self) -> broadcast::Receiver<AgentEvent>;
    /// Queue a user prompt for the next turn. No-op once the session ends.
    fn send_input(&self, prompt: String);
    /// Interrupt the in-flight turn. No-op when idle or finished.
    fn cancel(&self);
}

/// A live, replayable view of one session: an append-only event log plus a
/// broadcast stream, with a steering channel back to the agent. Cheap to
/// clone (all state is shared).
#[derive(Clone)]
pub struct LiveSession {
    log: Arc<Mutex<Vec<AgentEvent>>>,
    events: broadcast::Sender<AgentEvent>,
    steer: mpsc::UnboundedSender<Steer>,
}

/// The recording end a session runner hands to its agent: every emitted
/// event lands in the log and on the broadcast stream.
#[derive(Clone)]
pub struct SessionSink {
    session: LiveSession,
}

/// The steering end a session runner polls between (and during) turns.
pub struct SessionSource {
    inbox: mpsc::UnboundedReceiver<Steer>,
}

/// Create the three ends of a session channel: the handle frontends hold,
/// the sink the agent emits into, and the source the runner steers from.
pub fn session_channel() -> (LiveSession, SessionSink, SessionSource) {
    let (events, _) = broadcast::channel(SESSION_EVENT_CAPACITY);
    let (steer, inbox) = mpsc::unbounded_channel();
    let session = LiveSession {
        log: Arc::new(Mutex::new(Vec::new())),
        events,
        steer,
    };
    (
        session.clone(),
        SessionSink { session },
        SessionSource { inbox },
    )
}

impl SessionHandle for LiveSession {
    fn snapshot(&self) -> Vec<AgentEvent> {
        self.log.lock().unwrap().clone()
    }

    fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.events.subscribe()
    }

    fn send_input(&self, prompt: String) {
        let _ = self.steer.send(Steer::Prompt(prompt));
    }

    fn cancel(&self) {
        let _ = self.steer.send(Steer::Cancel);
    }
}

impl SessionSink {
    /// Record and broadcast one event. No receivers is fine — the log
    /// already holds the record.
    pub fn emit(&self, event: AgentEvent) {
        self.session.log.lock().unwrap().push(event.clone());
        let _ = self.session.events.send(event);
    }

    /// Whether anyone can still observe this session (handle alive).
    pub fn is_alive(&self) -> bool {
        self.session.events.receiver_count() > 0
    }
}

impl SessionSource {
    /// Wait for the next steering message. `None` means every handle was
    /// dropped and the session should shut down.
    pub async fn recv(&mut self) -> Option<Steer> {
        self.inbox.recv().await
    }

    /// Drain already-arrived steering messages without blocking.
    pub fn try_recv(&mut self) -> Option<Steer> {
        self.inbox.try_recv().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_replays_and_stream_follows() {
        let (handle, sink, _source) = session_channel();
        sink.emit(AgentEvent::AssistantText("one".into()));
        let mut stream = handle.subscribe();
        sink.emit(AgentEvent::AssistantText("two".into()));
        // A late joiner sees the full log plus can follow the stream.
        assert_eq!(
            handle.snapshot(),
            vec![
                AgentEvent::AssistantText("one".into()),
                AgentEvent::AssistantText("two".into()),
            ]
        );
        assert_eq!(
            stream.try_recv().unwrap(),
            AgentEvent::AssistantText("two".into())
        );
    }

    #[tokio::test]
    async fn steering_flows_from_handle_to_source() {
        let (handle, sink, mut source) = session_channel();
        handle.send_input("next".into());
        handle.cancel();
        assert_eq!(source.recv().await, Some(Steer::Prompt("next".into())));
        assert_eq!(source.recv().await, Some(Steer::Cancel));
        // Dropping the whole frontend side closes the source.
        drop(handle);
        drop(sink);
        assert_eq!(source.recv().await, None);
    }
}
