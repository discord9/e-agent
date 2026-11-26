//! Stub ACP agent tests: no real login is available here, so every test runs
//! a `/bin/bash -c` script that speaks the same newline-delimited JSON-RPC on
//! stdin/stdout. `$1` is a log file where the stub appends every frame it
//! received (and the client's permission answers), `$2` selects whether
//! `session/load` succeeds (`ok`) or fails (`fail`). The tests therefore
//! assert the exact wire sequence, the reuse decision, and what the tool
//! accumulated.

use super::*;
use crate::config::DEFAULT_ACP_ALLOW_KINDS;

/// Happy-path stub: answers `initialize` / `session/new` / `session/load` /
/// `session/set_model`, then on `session/prompt` streams two
/// `agent_message_chunk`s with one `tool_call` + one `usage_update` + one
/// `agent_thought_chunk` between them, asks for permission twice (a `read`,
/// then an `execute`) and logs both answers, and resolves the prompt with
/// `end_turn`.
///
/// `session/new` ids are numbered from the log, so one log file yields
/// distinct ids across calls (`sess-new-1`, `sess-new-2`, …).
const STUB: &str = r#"
log="$1"
loadmode="$2"
tag="$3"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  printf '%s\n' "$line" >> "$log"
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true},"agentInfo":{"name":"stub-acp","version":"9.9.9"},"authMethods":[]}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      n=$(grep -c '"method":"session/new"' "$log")
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"sess-%s-%s","models":[]}}\n' "$id" "$tag" "$n"
      ;;
    *'"method":"session/load"'*)
      sid=$(printf '%s' "$line" | grep -o '"sessionId":"[^"]*"' | head -1 | cut -d'"' -f4)
      if [ "$loadmode" = "fail" ]; then
        printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32001,"message":"session not found"}}\n' "$id"
      else
        printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"%s"}}\n' "$id" "$sid"
      fi
      ;;
    *'"method":"session/set_model"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
      ;;
    *'"method":"session/prompt"'*)
      sid=$(printf '%s' "$line" | grep -o '"sessionId":"[^"]*"' | head -1 | cut -d'"' -f4)
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"alpha "}}}}\n' "$sid"
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"tool_call","toolCallId":"t1","title":"run ls","kind":"execute","status":"pending"}}}\n' "$sid"
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"usage_update","used":10,"size":100}}}\n' "$sid"
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"secret-thought"}}}}\n' "$sid"
      printf '{"jsonrpc":"2.0","id":9001,"method":"session/request_permission","params":{"sessionId":"%s","toolCall":{"toolCallId":"r1","title":"read notes.md","kind":"read"},"options":[{"optionId":"allow-1","name":"Allow once","kind":"allow_once"},{"optionId":"reject-1","name":"Reject","kind":"reject_once"}]}}\n' "$sid"
      IFS= read -r answer
      printf '%s\n' "$answer" >> "$log"
      printf '{"jsonrpc":"2.0","id":9002,"method":"session/request_permission","params":{"sessionId":"%s","toolCall":{"toolCallId":"t1","title":"run ls","kind":"execute","rawInput":{"command":"ls -la"}},"options":[{"optionId":"allow-1","name":"Allow once","kind":"allow_once"},{"optionId":"reject-1","name":"Reject","kind":"reject_once"}]}}\n' "$sid"
      IFS= read -r answer
      printf '%s\n' "$answer" >> "$log"
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"beta"}}}}\n' "$sid"
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
  esac
done
"#;

/// Answers `initialize` the way `agy acp` does without a login.
const AUTH_STUB: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32000,"message":"Authentication required"}}\n' "$id"
      ;;
  esac
done
"#;

/// Streams only thought text, so the fallback body is exercised.
const THOUGHT_STUB: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{},"agentInfo":{"name":"stub-acp","version":"9.9.9"},"authMethods":[]}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"sess-thought"}}\n' "$id"
      ;;
    *'"method":"session/prompt"'*)
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"only a thought"}}}}\n'
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
  esac
done
"#;

/// Never answers and keeps appending to `$1`, so a cancelled call is visible
/// as a heartbeat file that stops growing.
const HANG_STUB: &str = r#"
beat="$1"
( while :; do printf 'x' >> "$beat"; sleep 0.05; done ) &
while IFS= read -r line; do :; done
"#;

fn stub_config(script: &str, log: &Path, load: &str) -> AcpConfig {
    tagged_config(script, log, load, "new")
}

/// Same stub, with a tag so two tools using separate logs get distinct ids.
fn tagged_config(script: &str, log: &Path, load: &str, tag: &str) -> AcpConfig {
    AcpConfig {
        binary: "/bin/bash".into(),
        args: vec![
            "-c".into(),
            script.into(),
            "acp-stub".into(),
            log.display().to_string(),
            load.into(),
            tag.into(),
        ],
        model: None,
        allow_kinds: DEFAULT_ACP_ALLOW_KINDS
            .iter()
            .map(|kind| (*kind).to_owned())
            .collect(),
        danger: false,
    }
}

/// A tool bound to `session`, backed by a JSONL session store rooted at
/// `root`. A metadata row is created first, because the store's ACP write
/// never self-creates (`create_meta` is what the session factory does).
async fn tool_with(root: &Path, config: AcpConfig, session: &str) -> Box<dyn Tool> {
    let store = SessionStore::Jsonl;
    store
        .create_meta(root, session, Some("test-model"), None, None, None, None)
        .await
        .expect("create the session metadata row");
    Box::new(AcpPrompt {
        workspace: Workspace::new(root).expect("workspace"),
        config,
        store,
        root: root.to_path_buf(),
        session: session.to_owned(),
        cached: StdMutex::new(HashMap::new()),
    })
}

/// The stored ACP session id for `session`.
async fn stored(root: &Path, session: &str) -> Option<String> {
    SessionStore::Jsonl
        .acp_session_id(root, session)
        .await
        .expect("read the stored ACP session id")
}

fn logged_frames(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .expect("the stub wrote its frame log")
        .lines()
        .map(str::to_owned)
        .collect()
}

fn count(frames: &[String], needle: &str) -> usize {
    frames.iter().filter(|frame| frame.contains(needle)).count()
}

#[tokio::test]
async fn handshake_prompt_and_kind_policy_are_recorded() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let tool = tool_with(&root, stub_config(STUB, &log, "ok"), "session-a").await;

    let output = tool
        .execute(json!({"prompt": "list the files"}))
        .await
        .expect("the stub resolves the prompt");

    // Wire sequence: initialize -> session/new -> session/prompt, then the
    // client's answers to the two permission requests. `session/list` is
    // never used: reuse is by the session store, never by guessing.
    let frames = logged_frames(&log);
    assert_eq!(frames.len(), 5, "unexpected frames: {}", frames.join("\n"));
    assert!(
        frames[0].contains(r#""method":"initialize""#),
        "{}",
        frames[0]
    );
    assert!(
        frames[0].contains(r#""protocolVersion":1"#),
        "{}",
        frames[0]
    );
    assert!(
        frames[0].contains(r#""clientCapabilities":{}"#),
        "{}",
        frames[0]
    );
    assert!(
        frames[1].contains(r#""method":"session/new""#),
        "{}",
        frames[1]
    );
    assert!(
        frames[1].contains(&format!(r#""cwd":"{}""#, root.display())),
        "{}",
        frames[1]
    );
    assert!(frames[1].contains(r#""mcpServers":[]"#), "{}", frames[1]);
    assert!(
        frames[2].contains(r#""method":"session/prompt""#),
        "{}",
        frames[2]
    );
    assert!(
        frames[2].contains(r#""text":"list the files""#),
        "{}",
        frames[2]
    );
    // `read` is in the default allow_kinds: "allow once" and NO receipt line.
    assert!(
        frames[3].contains(r#""outcome":"selected""#),
        "{}",
        frames[3]
    );
    assert!(
        frames[3].contains(r#""optionId":"allow-1""#),
        "{}",
        frames[3]
    );
    // `execute` is not: denied.
    assert!(
        frames[4].contains(r#""outcome":"cancelled""#),
        "{}",
        frames[4]
    );

    let content = output.content;
    assert!(
        content.starts_with("acp: stub-acp 9.9.9 · session sess-new-1\n"),
        "{content}"
    );
    assert!(
        content.contains(
            "[acp] no saved ACP session for this e-agent session; started a new session sess-new-1"
        ),
        "{content}"
    );
    assert!(content.contains("alpha beta"), "{content}");
    assert!(
        !content.contains("secret-thought"),
        "thought text must not be the body when assistant text exists: {content}"
    );
    assert!(content.contains("stopReason: end_turn"), "{content}");
    assert!(content.contains("\ndenied execute: run ls"), "{content}");
    assert!(
        !content.contains("read notes.md"),
        "an allowed read stays silent: {content}"
    );
    assert!(content.contains("ignored updates: 2"), "{content}");
    // The id is in this session's own metadata row, and nothing else.
    assert_eq!(
        stored(&root, "session-a").await.as_deref(),
        Some("sess-new-1")
    );
}

#[tokio::test]
async fn a_null_row_starts_a_new_session_and_stores_the_fresh_id() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    // The metadata row exists (create_meta) but carries no ACP id: exactly a
    // pre-existing session from before the column shipped.
    assert_eq!(stored(&root, "session-a").await, None);
    let tool = tool_with(&root, stub_config(STUB, &log, "ok"), "session-a").await;

    tool.execute(json!({"prompt": "first"})).await.unwrap();

    assert_eq!(count(&logged_frames(&log), r#""method":"session/new""#), 1);
    assert_eq!(
        stored(&root, "session-a").await.as_deref(),
        Some("sess-new-1")
    );
}

#[tokio::test]
async fn a_second_call_reuses_the_mapped_session() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let tool = tool_with(&root, stub_config(STUB, &log, "ok"), "session-a").await;

    tool.execute(json!({"prompt": "first"})).await.unwrap();
    let second = tool.execute(json!({"prompt": "second"})).await.unwrap();

    let frames = logged_frames(&log);
    assert_eq!(count(&frames, r#""method":"session/new""#), 1);
    assert_eq!(count(&frames, r#""method":"session/load""#), 1);
    let load = frames
        .iter()
        .find(|frame| frame.contains(r#""method":"session/load""#))
        .unwrap();
    assert!(load.contains(r#""sessionId":"sess-new-1""#), "{load}");
    assert_eq!(count(&frames, r#""method":"session/list""#), 0);
    // A plain reuse carries no notice.
    assert!(
        second
            .content
            .starts_with("acp: stub-acp 9.9.9 · session sess-new-1\n"),
        "{}",
        second.content
    );
    assert!(!second.content.contains("[acp] "), "{}", second.content);
}

#[tokio::test]
async fn an_restart_reads_the_id_back_through_the_store() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let first = tool_with(&root, stub_config(STUB, &log, "ok"), "session-a").await;
    first.execute(json!({"prompt": "before"})).await.unwrap();
    // A restart: the in-process cache is gone, the stored row is not.
    assert_eq!(
        stored(&root, "session-a").await.as_deref(),
        Some("sess-new-1")
    );
    drop(first);

    let restarted = tool_with(&root, stub_config(STUB, &log, "ok"), "session-a").await;
    let output = restarted.execute(json!({"prompt": "after"})).await.unwrap();

    let frames = logged_frames(&log);
    assert_eq!(count(&frames, r#""method":"session/new""#), 1);
    let load = frames
        .iter()
        .find(|frame| frame.contains(r#""method":"session/load""#))
        .unwrap();
    assert!(load.contains(r#""sessionId":"sess-new-1""#), "{load}");
    assert!(
        output
            .content
            .starts_with("acp: stub-acp 9.9.9 · session sess-new-1\n"),
        "{}",
        output.content
    );
    assert!(!output.content.contains("[acp] "), "{}", output.content);
}

#[tokio::test]
async fn two_sessions_in_one_workspace_never_cross_load() {
    let temp = tempfile::tempdir().unwrap();
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let log_a = temp.path().join("a.log");
    let log_b = temp.path().join("b.log");
    let session_a = tool_with(&root, tagged_config(STUB, &log_a, "ok", "a"), "session-a").await;
    let session_b = tool_with(&root, tagged_config(STUB, &log_b, "ok", "b"), "session-b").await;

    session_a.execute(json!({"prompt": "a1"})).await.unwrap();
    session_b.execute(json!({"prompt": "b1"})).await.unwrap();
    let a2 = session_a.execute(json!({"prompt": "a2"})).await.unwrap();
    let b2 = session_b.execute(json!({"prompt": "b2"})).await.unwrap();

    // Each session created and then reloads its OWN id.
    let frames_a = logged_frames(&log_a);
    let frames_b = logged_frames(&log_b);
    assert_eq!(count(&frames_a, r#""method":"session/new""#), 1);
    assert_eq!(count(&frames_b, r#""method":"session/new""#), 1);
    let load_a = frames_a
        .iter()
        .find(|frame| frame.contains(r#""method":"session/load""#))
        .unwrap();
    let load_b = frames_b
        .iter()
        .find(|frame| frame.contains(r#""method":"session/load""#))
        .unwrap();
    assert!(load_a.contains("sess-a-1"), "{load_a}");
    assert!(load_b.contains("sess-b-1"), "{load_b}");
    assert!(a2.content.contains("session sess-a-1"), "{}", a2.content);
    assert!(b2.content.contains("session sess-b-1"), "{}", b2.content);
    // Two rows, two ids, no cross-talk.
    assert_eq!(
        stored(&root, "session-a").await.as_deref(),
        Some("sess-a-1")
    );
    assert_eq!(
        stored(&root, "session-b").await.as_deref(),
        Some("sess-b-1")
    );
}

#[tokio::test]
async fn a_changed_workspace_starts_a_new_session_and_says_so() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    std::fs::create_dir(root.join("sub")).unwrap();
    let tool = tool_with(&root, stub_config(STUB, &log, "ok"), "session-a").await;

    tool.execute(json!({"prompt": "first"})).await.unwrap();
    let output = tool
        .execute(json!({"prompt": "second", "workspace": "sub"}))
        .await
        .unwrap();

    let frames = logged_frames(&log);
    assert_eq!(count(&frames, r#""method":"session/new""#), 2);
    assert_eq!(count(&frames, r#""method":"session/load""#), 0);
    assert!(
        output
            .content
            .starts_with("acp: stub-acp 9.9.9 · session sess-new-2\n"),
        "{}",
        output.content
    );
    assert!(
        output.content.contains("[acp] workspace changed from "),
        "{}",
        output.content
    );
    assert!(
        output
            .content
            .contains("sub; started a new session sess-new-2"),
        "{}",
        output.content
    );
    // The stored id follows the new conversation.
    assert_eq!(
        stored(&root, "session-a").await.as_deref(),
        Some("sess-new-2")
    );
}

#[tokio::test]
async fn a_failed_load_starts_over_and_names_the_abandoned_id() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let tool = tool_with(&root, stub_config(STUB, &log, "fail"), "session-a").await;

    tool.execute(json!({"prompt": "first"})).await.unwrap();
    let output = tool.execute(json!({"prompt": "second"})).await.unwrap();

    let frames = logged_frames(&log);
    assert_eq!(count(&frames, r#""method":"session/load""#), 1);
    // The load failed, so a new session was created instead.
    assert_eq!(count(&frames, r#""method":"session/new""#), 2);
    assert!(
        output
            .content
            .contains("[acp] could not resume session sess-new-1 (load failed: "),
        "{}",
        output.content
    );
    assert!(
        output.content.contains("started a new session sess-new-2"),
        "{}",
        output.content
    );
    assert!(output.content.contains("alpha beta"), "{}", output.content);
    // The fallback id replaces the abandoned one.
    assert_eq!(
        stored(&root, "session-a").await.as_deref(),
        Some("sess-new-2")
    );
}

#[tokio::test]
async fn fresh_forces_a_new_session_even_with_a_mapping() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let tool = tool_with(&root, stub_config(STUB, &log, "ok"), "session-a").await;

    tool.execute(json!({"prompt": "first"})).await.unwrap();
    let output = tool
        .execute(json!({"prompt": "second", "fresh": true}))
        .await
        .unwrap();

    let frames = logged_frames(&log);
    assert_eq!(count(&frames, r#""method":"session/new""#), 2);
    assert_eq!(count(&frames, r#""method":"session/load""#), 0);
    assert!(
        output
            .content
            .starts_with("acp: stub-acp 9.9.9 · session sess-new-2\n"),
        "{}",
        output.content
    );
    assert_eq!(
        stored(&root, "session-a").await.as_deref(),
        Some("sess-new-2")
    );
}

#[tokio::test]
async fn danger_allows_every_kind_and_says_so_once() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let mut config = stub_config(STUB, &log, "ok");
    config.danger = true;
    let tool = tool_with(&root, config, "session-a").await;

    let output = tool.execute(json!({"prompt": "hi"})).await.unwrap();

    let frames = logged_frames(&log);
    let answers: Vec<&String> = frames
        .iter()
        .filter(|frame| frame.contains(r#""outcome""#))
        .collect();
    assert_eq!(answers.len(), 2, "both requests were answered");
    for answer in answers {
        assert!(answer.contains(r#""outcome":"selected""#), "{answer}");
    }
    assert_eq!(
        output
            .content
            .matches("danger: allowed every permission request")
            .count(),
        1,
        "{}",
        output.content
    );
    assert!(!output.content.contains("denied "), "{}", output.content);
}

#[tokio::test]
async fn empty_allow_kinds_denies_everything() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let mut config = stub_config(STUB, &log, "ok");
    config.allow_kinds = Vec::new();
    let tool = tool_with(&root, config, "session-a").await;

    let output = tool.execute(json!({"prompt": "hi"})).await.unwrap();

    let frames = logged_frames(&log);
    let answers: Vec<&String> = frames
        .iter()
        .filter(|frame| frame.contains(r#""outcome""#))
        .collect();
    assert_eq!(answers.len(), 2);
    for answer in answers {
        assert!(answer.contains(r#""outcome":"cancelled""#), "{answer}");
    }
    assert!(
        output.content.contains("\ndenied read: read notes.md"),
        "{}",
        output.content
    );
    assert!(
        output.content.contains("\ndenied execute: run ls"),
        "{}",
        output.content
    );
}

#[tokio::test]
async fn model_is_applied_through_session_set_model() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let mut config = stub_config(STUB, &log, "ok");
    config.model = Some("stub-model-1".into());
    let tool = tool_with(&root, config, "session-a").await;

    tool.execute(json!({"prompt": "hi"})).await.unwrap();

    let frames = logged_frames(&log);
    assert!(
        frames[2].contains(r#""method":"session/set_model""#),
        "{}",
        frames[2]
    );
    assert!(
        frames[2].contains(r#""modelId":"stub-model-1""#),
        "{}",
        frames[2]
    );
    assert!(
        frames[3].contains(r#""method":"session/prompt""#),
        "{}",
        frames[3]
    );
}

#[tokio::test]
async fn thought_text_is_the_body_when_no_message_chunk_arrives() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let tool = tool_with(&root, stub_config(THOUGHT_STUB, &log, "ok"), "session-a").await;

    let output = tool.execute(json!({"prompt": "think"})).await.unwrap();

    assert!(
        output.content.contains("only a thought"),
        "{}",
        output.content
    );
    assert!(
        output.content.contains("stopReason: end_turn"),
        "{}",
        output.content
    );
}

#[tokio::test]
async fn authentication_required_becomes_a_clean_tool_error() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let tool = tool_with(&root, stub_config(AUTH_STUB, &log, "ok"), "session-a").await;

    let error = tool
        .execute(json!({"prompt": "hi"}))
        .await
        .expect_err("an unauthenticated agent fails the call");

    assert!(
        error.contains("requires authentication"),
        "the error must name the auth problem: {error}"
    );
    assert!(error.contains("Authentication required"), "{error}");
    assert!(!error.contains("jsonrpc"), "{error}");
}

#[tokio::test]
async fn cancel_kills_the_agent_process_group() {
    let temp = tempfile::tempdir().unwrap();
    let beat = temp.path().join("beat.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let tool = tool_with(&root, stub_config(HANG_STUB, &beat, "ok"), "session-a").await;

    // The deadline drops the tool future, which is exactly what a cancelled
    // turn does to it.
    let cancelled = tokio::time::timeout(
        Duration::from_millis(800),
        tool.execute(json!({"prompt": "hang"})),
    )
    .await;
    assert!(cancelled.is_err(), "the hanging stub cannot answer");

    let before = beat_size(&beat);
    assert!(before > 0, "the stub never started its heartbeat");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        beat_size(&beat),
        before,
        "the agent process group kept running after the cancel"
    );
}

fn beat_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

#[tokio::test]
async fn empty_prompt_is_rejected_before_spawning() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let tool = tool_with(&root, stub_config(STUB, &log, "ok"), "session-a").await;

    let error = tool
        .execute(json!({"prompt": "   "}))
        .await
        .expect_err("a blank prompt is refused");
    assert!(error.contains("non-empty"), "{error}");
    assert!(!log.exists(), "nothing may be spawned for an empty prompt");
}

#[tokio::test]
async fn workspace_outside_the_root_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let tool = tool_with(&root, stub_config(STUB, &log, "ok"), "session-a").await;

    let error = tool
        .execute(json!({"prompt": "hi", "workspace": "/tmp"}))
        .await
        .expect_err("an outside workspace is refused");
    assert!(error.contains("invalid `workspace`"), "{error}");
    assert!(!log.exists(), "nothing may be spawned for a bad workspace");
}

#[test]
fn push_bounded_stops_on_a_char_boundary() {
    let mut buffer = String::new();
    assert!(!push_bounded(&mut buffer, "ab", 8));
    assert!(push_bounded(&mut buffer, "测测测测", 8));
    assert_eq!(buffer, "ab测测");
    assert!(push_bounded(&mut buffer, "more", 8));
    assert_eq!(buffer, "ab测测");
}

#[test]
fn summaries_prefer_the_title_then_the_command() {
    let title = json!({"title": "run ls", "kind": "execute", "rawInput": {"command": "ls -la"}});
    assert_eq!(tool_call_summary(Some(&title)), "run ls");
    let command = json!({"kind": "execute", "rawInput": {"command": "ls -la"}});
    assert_eq!(tool_call_summary(Some(&command)), "ls -la");
    assert_eq!(tool_call_summary(None), "unknown tool call");
}
