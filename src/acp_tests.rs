//! Tests for the ACP session driver: launch resolution, the permission
//! question (identity + verbatim option ids), turn streaming, and the two ways
//! a turn must fail or end (process death, explicit cancel).
//!
//! The agent under test is a small POSIX `sh` stub that speaks the protocol on
//! stdin/stdout and logs every frame it receives, so the assertions are about
//! the WIRE, not about a mock of our own code.

use super::*;
use crate::agent::{Message, SessionEntry};
use crate::config::{AcpConfig, SessionBackend};
use crate::runner::{IdlePolicy, PromptSubmission, SessionRunner, SessionStatus};
use crate::session_store::SessionStore;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const STUB: &str = r#"#!/bin/sh
# Stub ACP agent. $1 = frame log, $2 = scenario.
log="$1"
scenario="$2"
id_of() { printf '%s' "$1" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'; }
prompt_id=""
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$log"
  case "$line" in
    *'"method":"initialize"'*)
      id=$(id_of "$line")
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentInfo":{"name":"stub-agent","version":"1"},"agentCapabilities":{}}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      id=$(id_of "$line")
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"sess-1"}}\n' "$id"
      ;;
    *'"method":"session/prompt"'*)
      prompt_id=$(id_of "$line")
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"thinking"}}}}\n'
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"tool_call","toolCallId":"c1","title":"ls","kind":"execute"}}}\n'
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello "}}}}\n'
      if [ "$scenario" = "die" ]; then
        # The agent dies mid-turn: no prompt response will ever come.
        exit 3
      fi
      if [ "$scenario" = "ask" ]; then
        printf '{"jsonrpc":"2.0","id":"perm-1","method":"session/request_permission","params":{"sessionId":"sess-1","toolCall":{"toolCallId":"call_9","title":"ls -la","kind":"execute","rawInput":{"command":"ls -la"}},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},{"optionId":"deny","name":"Deny","kind":"reject_once"}]}}\n'
        continue
      fi
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"world"}}}}\n'
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$prompt_id"
      ;;
    *'"id":"perm-1"'*)
      # The permission decision: record it, then finish the turn.
      printf '%s\n' "$line" >> "$log.decision"
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"world"}}}}\n'
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$prompt_id"
      ;;
    *'"method":"session/cancel"'*) : ;;
  esac
done
"#;

#[cfg(unix)]
fn stub(root: &Path, scenario: &str) -> (AgentLaunch, std::path::PathBuf) {
    let script = root.join(format!("stub-{scenario}.sh"));
    std::fs::write(&script, STUB).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let log = root.join(format!("frames-{scenario}.log"));
    let launch = AgentLaunch {
        binary: script.display().to_string(),
        args: vec![log.display().to_string(), scenario.to_owned()],
        model: None,
    };
    (launch, log)
}

#[cfg(unix)]
fn frames(log: &Path) -> String {
    std::fs::read_to_string(log).unwrap_or_default()
}

#[cfg(unix)]
fn decisions(log: &Path) -> String {
    std::fs::read_to_string(format!("{}.decision", log.display())).unwrap_or_default()
}

/// Poll the session status until it matches, then return it.
#[cfg(unix)]
async fn wait_status(
    handle: &crate::runner::SessionHandle,
    ready: impl Fn(&SessionStatus) -> bool,
) -> SessionStatus {
    let status = handle.status();
    for _ in 0..600 {
        let current = status.borrow().clone();
        if ready(&current) {
            return current;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("session never reached the expected status");
}

/// Poll the session's event log until `ready` matches one of its events.
#[cfg(unix)]
async fn wait_event(
    handle: &crate::runner::SessionHandle,
    ready: impl Fn(&crate::agent::AgentEvent) -> bool,
) {
    for _ in 0..600 {
        if handle.snapshot().iter().any(&ready) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("event never appeared; log was {:?}", handle.snapshot());
}

#[cfg(unix)]
async fn wait_for_log(log: &Path, needle: &str) {
    for _ in 0..600 {
        if frames(log).contains(needle) || decisions(log).contains(needle) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!(
        "frame {needle:?} never reached the agent; log was:\n{}\n{}",
        frames(log),
        decisions(log)
    );
}

/// `wait_for_log` with the session state attached, for turn-boundary failures.
#[cfg(unix)]
async fn wait_for_log_or_report(handle: &crate::runner::SessionHandle, log: &Path, needle: &str) {
    for _ in 0..600 {
        if frames(log).contains(needle) || decisions(log).contains(needle) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let status = handle.status();
    let current = status.borrow().clone();
    panic!(
        "frame {needle:?} never reached the agent; status = {current:?}; events = {:?}; log =\n{}",
        handle.snapshot(),
        frames(log)
    );
}

// ~~~~~~~~~~~~~~~~~~~~~~~~ launch + question mapping (pure) ~~~~~~~~~~~~~~~~

#[test]
fn an_agent_name_resolves_to_the_configured_agent_or_a_bare_program() {
    // No `[acp]` section: the name IS the program, with stdio arguments.
    let launch = AgentLaunch::resolve("agy", None).unwrap();
    assert_eq!(launch.binary, "agy");
    assert_eq!(
        launch.args,
        crate::config::DEFAULT_ACP_ARGS
            .iter()
            .map(|arg| (*arg).to_owned())
            .collect::<Vec<_>>()
    );
    assert_eq!(launch.model, None);

    // The configured agent wins when the name matches it (path, args, model).
    let configured = AcpConfig {
        binary: "/opt/agy".into(),
        args: vec!["acp".into(), "--stdio".into(), "--agent".into(), "x".into()],
        model: Some("fast".into()),
    };
    let launch = AgentLaunch::resolve("agy", Some(&configured)).unwrap();
    assert_eq!(launch.binary, "/opt/agy");
    assert_eq!(launch.args.len(), 4);
    assert_eq!(launch.model.as_deref(), Some("fast"));

    // A DIFFERENT name is not silently mapped onto the configured program.
    let launch = AgentLaunch::resolve("other", Some(&configured)).unwrap();
    assert_eq!(launch.binary, "other");
    assert_eq!(launch.model, None);

    // A name is a program NAME: a path is refused (it lives in `[acp]`), and so
    // is anything that is not a bare token.
    assert!(AgentLaunch::resolve("/bin/sh", None).is_err());
    assert!(AgentLaunch::resolve("sh -c rm", None).is_err());
    assert!(AgentLaunch::resolve("", None).is_err());
}

#[test]
fn a_permission_identity_is_unique_per_request_and_covers_the_action() {
    let params = json!({
        "toolCall": {"toolCallId": "call_1", "title": "ls", "kind": "execute"},
        "options": [{"optionId": "allow", "kind": "allow_once"}],
    });
    let first = permission_identity(&params);
    let second = permission_identity(&params);
    assert_ne!(first, second, "every ask has its own identity");
    assert!(first.starts_with("acp-permission-"));
    assert!(first.contains("call_1"));

    // A different request (other tool call / other command / other choices)
    // never shares an identity with the first one.
    let other = json!({
        "toolCall": {"toolCallId": "call_2", "title": "rm", "kind": "execute"},
        "options": [{"optionId": "allow", "kind": "allow_once"}],
    });
    assert!(!permission_identity(&other).contains("call_1"));
}

#[test]
fn permission_options_and_prompt_map_one_to_one_from_the_request() {
    let params = json!({
        "toolCall": {"toolCallId": "c1", "title": "Run ls", "kind": "execute",
                     "rawInput": {"command": "ls -la"}},
        "options": [
            {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
            {"optionId": "deny", "name": "Deny", "kind": "reject_once"},
            {"name": "no id, dropped", "kind": "allow_once"}
        ],
    });
    let options = question_options(&params).unwrap();
    assert_eq!(
        options
            .iter()
            .map(|option| (option.id.as_str(), option.label.as_str()))
            .collect::<Vec<_>>(),
        vec![("allow", "Allow"), ("deny", "Deny")]
    );
    assert_eq!(options[0].description.as_deref(), Some("allow_once"));
    assert_eq!(permission_prompt(&params), "Run ls: ls -la");

    // No usable option id at all: nothing to ask.
    assert!(question_options(&json!({"options": []})).is_none());
    assert!(question_options(&json!({})).is_none());

    // A long command is summarised, not shown in full.
    let long = json!({"toolCall": {"title": "run", "rawInput": {"command": "x".repeat(500)}}});
    assert!(permission_prompt(&long).chars().count() < 300);
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~ driver turns ~~~~~~~~~~~~~~~~~~~~~~~~~~~~

/// A whole ACP session: prompt → streamed deltas → committed display text →
/// idle with the process still alive and the task still registered.
#[cfg(unix)]
#[tokio::test]
async fn a_turn_streams_the_agents_text_and_returns_the_session_to_idle() {
    let temp = tempfile::tempdir().unwrap();
    let (launch, log) = stub(temp.path(), "plain");
    let driver = AcpDriver::connect(&launch, temp.path()).await.unwrap();
    assert_eq!(driver.name(), "stub-agent");
    assert_eq!(driver.session_id(), "sess-1");

    let store = SessionStore::connect(&SessionBackend::Jsonl, temp.path(), "sub-acp")
        .await
        .unwrap();
    let (runner, handle) = SessionRunner::new_acp(
        driver,
        store.clone(),
        temp.path().to_path_buf(),
        "sub-acp".to_owned(),
        IdlePolicy::WaitForInput,
    );
    let _task = runner.start(None);
    handle.prompt("say hello");
    // The stub answers instantly, so `Busy` is too short-lived to poll for: the
    // assistant-text event marks the end of the turn's stream and `Idle` is the
    // turn boundary that follows it.
    wait_event(&handle, |event| {
        matches!(event, crate::agent::AgentEvent::AssistantText(text) if text == "hello world")
    })
    .await;
    wait_status(&handle, |status| matches!(status, SessionStatus::Idle)).await;

    // The wire saw ONE prompt for this turn, carrying the whole task.
    let log_text = frames(&log);
    assert_eq!(log_text.matches("session/prompt").count(), 1);
    assert!(log_text.contains("\"text\":\"say hello\""));

    // The transcript is OURS and holds the streamed text, not the tool card.
    let page = store
        .load_head_page_located(temp.path(), "sub-acp", Some(200))
        .await
        .unwrap();
    let entries = page.entries;
    assert!(
        matches!(&entries[0], SessionEntry::Message { message } if matches!(message, Message::User { content, .. } if content == "say hello"))
    );
    assert!(
        matches!(&entries[1], SessionEntry::Message { message } if matches!(message, Message::Assistant(message) if message.content.as_deref() == Some("hello world")))
    );

    // Live deltas reached the session's single fanout.
    let events = handle.snapshot();
    assert!(events.iter().any(|event| matches!(
        event,
        crate::agent::AgentEvent::AssistantDelta(chunk) if chunk == "hello "
    )));

    // Idle, not finished: the session stays conversable, and the second turn
    // reuses the SAME process instead of spawning another one.
    assert_eq!(
        handle.submit_prompt_with_call_id(None, "again".into()),
        PromptSubmission::Queued,
        "the idle ACP session must accept a second prompt"
    );
    wait_for_log_or_report(&handle, &log, "\"text\":\"again\"").await;
    let log_text = frames(&log);
    assert_eq!(
        log_text.matches("\"method\":\"initialize\"").count(),
        1,
        "one process per session: no second handshake"
    );
    assert_eq!(log_text.matches("session/prompt").count(), 2);
    assert_eq!(
        handle.submit_prompt_with_call_id(None, "third".into()),
        PromptSubmission::Queued
    );
}

/// The permission request is this session's OWN question: the answer is bound
/// to the published call id, must name an offered option, and reaches the wire
/// as the agent's own `optionId` verbatim.
#[cfg(unix)]
#[tokio::test]
async fn a_permission_request_asks_this_session_and_the_chosen_id_is_sent_verbatim() {
    let temp = tempfile::tempdir().unwrap();
    let (launch, log) = stub(temp.path(), "ask");
    let driver = AcpDriver::connect(&launch, temp.path()).await.unwrap();
    let store = SessionStore::connect(&SessionBackend::Jsonl, temp.path(), "sub-acp")
        .await
        .unwrap();
    let (runner, handle) = SessionRunner::new_acp(
        driver,
        store,
        temp.path().to_path_buf(),
        "sub-acp".to_owned(),
        IdlePolicy::WaitForInput,
    );
    let _task = runner.start(None);
    handle.prompt("run ls -la");

    let waiting = wait_status(&handle, |status| {
        matches!(status, SessionStatus::WaitingInput(_))
    })
    .await;
    let SessionStatus::WaitingInput(request) = waiting else {
        unreachable!()
    };
    assert!(request.call_id.starts_with("acp-permission-"));
    let question = &request.questions[0];
    assert_eq!(question.id, PERMISSION_QUESTION);
    assert_eq!(question.prompt, "ls -la: ls -la");
    assert_eq!(
        question
            .options
            .iter()
            .map(|option| option.id.as_str())
            .collect::<Vec<_>>(),
        vec!["allow", "deny"]
    );

    // A foreign call id is refused, not treated as a prompt.
    assert_eq!(
        handle.submit_prompt_with_call_id(Some("acp-permission-nope".into()), "allow".into()),
        PromptSubmission::Conflict
    );
    // An answer that names no offered option is refuted and the question
    // stays open.
    assert!(matches!(
        handle.submit_prompt_with_call_id(Some(request.call_id.clone()), "yolo".into()),
        PromptSubmission::Invalid(_)
    ));
    // The real answer (exactly as the web view submits it).
    assert_eq!(
        handle.submit_prompt_with_call_id(Some(request.call_id.clone()), "allow".into()),
        PromptSubmission::Answered
    );
    wait_for_log(&log, "\"optionId\":\"allow\"").await;

    // The turn then finishes normally, and the receipt is in our transcript.
    wait_status(&handle, |status| matches!(status, SessionStatus::Idle)).await;
    let page = handle.web_attach().await.unwrap();
    let entries = page.entries;
    assert!(
        entries.iter().any(|entry| matches!(entry, SessionEntry::Notice { text } if text.contains("[question acp-permission-") && text.contains("Selected option: allow (Allow)"))),
        "transcript was {entries:?}"
    );
    // The decision on the wire is exactly the id the agent offered.
    let decision = decisions(&log);
    assert!(decision.contains("\"outcome\":\"selected\""), "{decision}");
    assert!(decision.contains("\"optionId\":\"allow\""), "{decision}");
}

/// The agent dies mid-turn: the turn fails visibly, with ONE terminal report,
/// and the session does not hang on a pending permission request.
#[cfg(unix)]
#[tokio::test]
async fn a_dead_agent_fails_the_turn_with_one_terminal_report() {
    let temp = tempfile::tempdir().unwrap();
    let (launch, _log) = stub(temp.path(), "die");
    let driver = AcpDriver::connect(&launch, temp.path()).await.unwrap();
    let store = SessionStore::connect(&SessionBackend::Jsonl, temp.path(), "sub-acp")
        .await
        .unwrap();
    let (runner, handle) = SessionRunner::new_acp(
        driver,
        store.clone(),
        temp.path().to_path_buf(),
        "sub-acp".to_owned(),
        IdlePolicy::WaitForInput,
    );
    let task = runner.start(None);
    handle.prompt("doomed");
    let finished = wait_status(&handle, |status| {
        matches!(status, SessionStatus::Finished(_))
    })
    .await;
    let SessionStatus::Finished(result) = finished else {
        unreachable!()
    };
    assert!(
        matches!(&result, crate::runner::SessionResult::Failed(text) if text.contains("acp")),
        "unexpected result {result:?}"
    );
    // Exactly one terminal report in the transcript.
    let page = store
        .load_head_page_located(temp.path(), "sub-acp", Some(200))
        .await
        .unwrap();
    assert_eq!(
        page.entries
            .iter()
            .filter(|entry| matches!(entry, SessionEntry::Error { .. }))
            .count(),
        1,
        "entries: {:?}",
        page.entries
    );
    // The runner task ended on its own (no hang).
    task.join().await.unwrap();
}

/// Cancelling an idle ACP session ends it: the process is killed and reaped,
/// and the wrapper sees a cancellation (its one report to the spawner).
#[cfg(unix)]
#[tokio::test]
async fn an_idle_cancel_ends_the_session_and_kills_its_process() {
    let temp = tempfile::tempdir().unwrap();
    let (launch, _log) = stub(temp.path(), "plain");
    let driver = AcpDriver::connect(&launch, temp.path()).await.unwrap();
    let store = SessionStore::connect(&SessionBackend::Jsonl, temp.path(), "sub-acp")
        .await
        .unwrap();
    let (runner, handle) = SessionRunner::new_acp(
        driver,
        store,
        temp.path().to_path_buf(),
        "sub-acp".to_owned(),
        IdlePolicy::WaitForInput,
    );
    let task = runner.start(None);
    wait_status(&handle, |status| matches!(status, SessionStatus::Idle)).await;
    handle.cancel();
    let finished = wait_status(&handle, |status| {
        matches!(status, SessionStatus::Finished(_))
    })
    .await;
    assert!(matches!(
        finished,
        SessionStatus::Finished(crate::runner::SessionResult::Cancelled)
    ));
    task.join().await.unwrap();
}

/// A `web_attach` on an ACP session serves OUR display transcript (the web
/// child view needs this: it opens every live session with `?view=web`).
#[cfg(unix)]
#[tokio::test]
async fn web_attach_serves_the_acp_display_transcript() {
    let temp = tempfile::tempdir().unwrap();
    let (launch, _log) = stub(temp.path(), "plain");
    let driver = AcpDriver::connect(&launch, temp.path()).await.unwrap();
    let store = SessionStore::connect(&SessionBackend::Jsonl, temp.path(), "sub-acp")
        .await
        .unwrap();
    let (runner, handle) = SessionRunner::new_acp(
        driver,
        store,
        temp.path().to_path_buf(),
        "sub-acp".to_owned(),
        IdlePolicy::WaitForInput,
    );
    let _task = runner.start(None);
    // A fresh session still attaches (empty head), like any live session.
    let page = handle.web_attach().await.unwrap();
    assert!(page.entries.is_empty());
    assert!(page.locations.is_empty());
    assert_eq!(page.status, SessionStatus::Idle);
}
