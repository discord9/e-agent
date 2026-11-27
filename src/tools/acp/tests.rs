//! Stub ACP agent tests: no real login is available here, so every test runs
//! a `/bin/bash -c` script that speaks the same newline-delimited JSON-RPC on
//! stdin/stdout. `$1` is a log file where the stub appends every frame it
//! received (and the client's permission answers), `$2` selects whether
//! `session/load` succeeds (`ok`) or fails (`fail`). The tests therefore
//! assert the exact wire sequence, the reuse decision, and what the tool
//! accumulated.

use super::*;
use crate::agent::{
    Agent, AgentEvent, AssistantMessage, Message, Model, ModelDeltaKind, ToolCall, ToolOutput,
    ToolSpec, Usage,
};
use crate::config::DEFAULT_ACP_ALLOW_KINDS;
use crate::runner::{
    IdlePolicy, PromptSubmission, SessionResult, SessionRunner, SessionStatus, UserInputRequest,
    session_test_channel,
};
use crate::session_store::SessionStore;
use async_trait::async_trait;

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

/// Sends ONE permission request and exits without ever reading the answer:
/// the child dies with the request still pending.
const ASK_THEN_EXIT_STUB: &str = r#"
log="$1"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  printf '%s\n' "$line" >> "$log"
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true},"agentInfo":{"name":"stub-acp","version":"9.9.9"},"authMethods":[]}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"sess-exit-1"}}\n' "$id"
      ;;
    *'"method":"session/prompt"'*)
      printf '{"jsonrpc":"2.0","id":9001,"method":"session/request_permission","params":{"sessionId":"sess-exit-1","toolCall":{"toolCallId":"r1","title":"read notes.md","kind":"read"},"options":[{"optionId":"allow-1","name":"Allow once","kind":"allow_once"}]}}\n'
      exit 0
      ;;
  esac
done
"#;

/// Advertises NO capabilities (`agentCapabilities` is empty), so
/// `session/load` is not part of the negotiation. `session/load` is answered
/// anyway, so a call must be visible in the log if one is (wrongly) made.
const NO_LOAD_STUB: &str = r#"
log="$1"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  printf '%s\n' "$line" >> "$log"
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{},"agentInfo":{"name":"stub-acp","version":"9.9.9"},"authMethods":[]}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      n=$(grep -c '"method":"session/new"' "$log")
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"sess-noload-%s"}}\n' "$id" "$n"
      ;;
    *'"method":"session/load"'*)
      sid=$(printf '%s' "$line" | grep -o '"sessionId":"[^"]*"' | head -1 | cut -d'"' -f4)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"%s"}}\n' "$id" "$sid"
      ;;
    *'"method":"session/prompt"'*)
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"fresh answer"}}}}\n'
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
  esac
done
"#;

/// Replays more restored history than [`TEXT_LIMIT`] allows as
/// `agent_message_chunk`s on `session/load`, then answers the load and streams
/// the real answer on the prompt.
const REPLAY_STUB: &str = r#"
log="$1"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  printf '%s\n' "$line" >> "$log"
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true},"agentInfo":{"name":"stub-acp","version":"9.9.9"},"authMethods":[]}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"sess-replay"}}\n' "$id"
      ;;
    *'"method":"session/load"'*)
      sid=$(printf '%s' "$line" | grep -o '"sessionId":"[^"]*"' | head -1 | cut -d'"' -f4)
      i=0
      while [ $i -lt 1500 ]; do
        printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"REPLAY-older-turn-"}}}}\n' "$sid"
        i=$((i+1))
      done
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"%s"}}\n' "$id" "$sid"
      ;;
    *'"method":"session/prompt"'*)
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"REAL ANSWER"}}}}\n'
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
  esac
done
"#;

/// Logs the cwd it was started in, then answers the handshake: proves which
/// directory a (cold) reuse actually runs in.
const CWD_STUB: &str = r#"
log="$1"
printf '{"stubPwd":"%s"}\n' "$(pwd)" >> "$log"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  printf '%s\n' "$line" >> "$log"
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true},"agentInfo":{"name":"stub-acp","version":"9.9.9"},"authMethods":[]}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      n=$(grep -c '"method":"session/new"' "$log")
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"sess-cwd-%s"}}\n' "$id" "$n"
      ;;
    *'"method":"session/load"'*)
      sid=$(printf '%s' "$line" | grep -o '"sessionId":"[^"]*"' | head -1 | cut -d'"' -f4)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"%s"}}\n' "$id" "$sid"
      ;;
    *'"method":"session/prompt"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
  esac
done
"#;

/// Asks ONE permission request (`allow_once` + `allow_always`) per prompt and
/// logs the answer: one run per stub process, so two runs in one e-agent
/// session reuse the same JSON-RPC id 9001.
const ONE_ASK_STUB: &str = r#"
log="$1"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  printf '%s\n' "$line" >> "$log"
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true},"agentInfo":{"name":"stub-acp","version":"9.9.9"},"authMethods":[]}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      n=$(grep -c '"method":"session/new"' "$log")
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"sess-one-%s"}}\n' "$id" "$n"
      ;;
    *'"method":"session/load"'*)
      sid=$(printf '%s' "$line" | grep -o '"sessionId":"[^"]*"' | head -1 | cut -d'"' -f4)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"%s"}}\n' "$id" "$sid"
      ;;
    *'"method":"session/prompt"'*)
      printf '{"jsonrpc":"2.0","id":9001,"method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"r1","title":"read notes.md","kind":"read"},"options":[{"optionId":"once-1","name":"Allow once","kind":"allow_once"},{"optionId":"always-1","name":"Always allow","kind":"allow_always"}]}}\n'
      IFS= read -r answer
      printf '%s\n' "$answer" >> "$log"
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"answered"}}}}\n'
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
  esac
done
"#;

/// Asks two permission requests: the first offers `allow_always` BEFORE
/// `allow_once`, the second offers them the other way round, so the answer's
/// `optionId` shows whether either kind was substituted for the other.
const TWO_ALLOW_STUB: &str = r#"
log="$1"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  printf '%s\n' "$line" >> "$log"
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true},"agentInfo":{"name":"stub-acp","version":"9.9.9"},"authMethods":[]}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"sess-two-allow"}}\n' "$id"
      ;;
    *'"method":"session/load"'*)
      sid=$(printf '%s' "$line" | grep -o '"sessionId":"[^"]*"' | head -1 | cut -d'"' -f4)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"%s"}}\n' "$id" "$sid"
      ;;
    *'"method":"session/prompt"'*)
      printf '{"jsonrpc":"2.0","id":9001,"method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"r1","title":"read notes.md","kind":"read"},"options":[{"optionId":"always-1","name":"Always allow","kind":"allow_always"},{"optionId":"once-1","name":"Allow once","kind":"allow_once"}]}}\n'
      IFS= read -r answer
      printf '%s\n' "$answer" >> "$log"
      printf '{"jsonrpc":"2.0","id":9002,"method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"r2","title":"read notes.md","kind":"read"},"options":[{"optionId":"once-2","name":"Allow once","kind":"allow_once"},{"optionId":"always-2","name":"Always allow","kind":"allow_always"}]}}\n'
      IFS= read -r answer
      printf '%s\n' "$answer" >> "$log"
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
  esac
done
"#;

/// Asks ONE permission request, then waits for the test's signal file
/// (`$1.die`) before exiting WITHOUT reading the answer: the child dies with
/// the request still pending, so only the runner can end the wait.
const ASK_THEN_DIE_STUB: &str = r#"
log="$1"
die="$1.die"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  printf '%s\n' "$line" >> "$log"
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true},"agentInfo":{"name":"stub-acp","version":"9.9.9"},"authMethods":[]}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"sess-die-1"}}\n' "$id"
      ;;
    *'"method":"session/prompt"'*)
      printf '{"jsonrpc":"2.0","id":9001,"method":"session/request_permission","params":{"sessionId":"sess-die-1","toolCall":{"toolCallId":"r1","title":"read notes.md","kind":"read"},"options":[{"optionId":"allow-1","name":"Allow once","kind":"allow_once"}]}}\n'
      while [ ! -f "$die" ]; do sleep 0.02; done
      exit 0
      ;;
  esac
done
"#;

/// Sends a `read` request, lets the human-facing side settle, then sends a
/// SECOND `read` request while the first is still awaiting an answer (both
/// `read`, i.e. kinds the no-frontend policy would allow), and logs both
/// answers.
const OVERLAP_STUB: &str = r#"
log="$1"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  printf '%s\n' "$line" >> "$log"
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true},"agentInfo":{"name":"stub-acp","version":"9.9.9"},"authMethods":[]}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"sess-overlap-1"}}\n' "$id"
      ;;
    *'"method":"session/prompt"'*)
      printf '{"jsonrpc":"2.0","id":9001,"method":"session/request_permission","params":{"sessionId":"sess-overlap-1","toolCall":{"toolCallId":"r1","title":"read notes.md","kind":"read"},"options":[{"optionId":"allow-1","name":"Allow once","kind":"allow_once"},{"optionId":"reject-1","name":"Reject","kind":"reject_once"}]}}\n'
      sleep 0.2
      printf '{"jsonrpc":"2.0","id":9002,"method":"session/request_permission","params":{"sessionId":"sess-overlap-1","toolCall":{"toolCallId":"t1","title":"read secrets.md","kind":"read"},"options":[{"optionId":"allow-1","name":"Allow once","kind":"allow_once"},{"optionId":"reject-1","name":"Reject","kind":"reject_once"}]}}\n'
      IFS= read -r answer
      printf '%s\n' "$answer" >> "$log"
      IFS= read -r answer
      printf '%s\n' "$answer" >> "$log"
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
  esac
done
"#;

/// Offers two options whose ids differ only by surrounding spaces: `" x "`
/// and `"x"`. The submission `"x"` must come back as the id `"x"`.
const COLLIDING_STUB: &str = r#"
log="$1"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  printf '%s\n' "$line" >> "$log"
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true},"agentInfo":{"name":"stub-acp","version":"9.9.9"},"authMethods":[]}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"sess-collide-1"}}\n' "$id"
      ;;
    *'"method":"session/prompt"'*)
      printf '{"jsonrpc":"2.0","id":9001,"method":"session/request_permission","params":{"sessionId":"sess-collide-1","toolCall":{"toolCallId":"r1","title":"read notes.md","kind":"read"},"options":[{"optionId":" x ","name":"Reject","kind":"reject_once"},{"optionId":"x","name":"Allow once","kind":"allow_once"}]}}\n'
      IFS= read -r answer
      printf '%s\n' "$answer" >> "$log"
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
  esac
done
"#;

/// Replays one chunk BEFORE the `session/load` response and one AFTER it (but
/// before the prompt goes out): only the pre-response chunk is restored
/// history.
const REPLAY_LATE_STUB: &str = r#"
log="$1"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  printf '%s\n' "$line" >> "$log"
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true},"agentInfo":{"name":"stub-acp","version":"9.9.9"},"authMethods":[]}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"sess-replay-late"}}\n' "$id"
      ;;
    *'"method":"session/load"'*)
      sid=$(printf '%s' "$line" | grep -o '"sessionId":"[^"]*"' | head -1 | cut -d'"' -f4)
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"REPLAY-older-turn-"}}}}\n' "$sid"
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"%s"}}\n' "$id" "$sid"
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"EARLY "}}}}\n' "$sid"
      ;;
    *'"method":"session/prompt"'*)
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"LATE"}}}}\n'
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
  esac
done
"#;

/// Answers `initialize` / `session/new`, replays one restored-history chunk on
/// `session/load` but NEVER answers it (the load is abandoned by its deadline),
/// then streams the real answer on the prompt: the shape where a stale replay
/// region would swallow the fallback session's chunks.
const LOAD_TIMEOUT_STUB: &str = r#"
log="$1"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  printf '%s\n' "$line" >> "$log"
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true},"agentInfo":{"name":"stub-acp","version":"9.9.9"},"authMethods":[]}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"sess-fresh-1"}}\n' "$id"
      ;;
    *'"method":"session/load"'*)
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-old","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"REPLAY-older-turn-"}}}}\n'
      ;;
    *'"method":"session/prompt"'*)
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-fresh-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"fresh answer"}}}}\n'
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
  esac
done
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
        handle: None,
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

/// The stub's frame log so far: an empty string until the stub writes it.
fn log_text(log: &Path) -> String {
    std::fs::read_to_string(log).unwrap_or_default()
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

// ---------------------------------------------------------------------------
// Permission requests become questions for the human when the session has a
// frontend; the chosen option id is what ACP receives back.
// ---------------------------------------------------------------------------

/// One model round that calls `acp_prompt`, then a final answer: a real runner
/// drives the tool, so the question path under test is the production one.
struct AcpCallModel {
    calls: usize,
}

#[async_trait]
impl Model for AcpCallModel {
    async fn complete(
        &mut self,
        _: &[Message],
        _: &[ToolSpec],
        _: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        self.calls += 1;
        let reply = if self.calls == 1 {
            AssistantMessage {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call-acp".into(),
                    name: "acp_prompt".into(),
                    arguments: json!({"prompt": "review the notes"}).to_string(),
                }],
                reasoning: None,
            }
        } else {
            AssistantMessage {
                content: Some("reviewed".into()),
                tool_calls: Vec::new(),
                reasoning: None,
            }
        };
        Ok((reply, None))
    }
}

/// Wait for the next blocking question the runner surfaces. The request the
/// test just answered (`after`) can still be the published status for a
/// moment, so it is skipped instead of being read twice.
async fn next_question(
    status: &mut tokio::sync::watch::Receiver<SessionStatus>,
    after: &str,
) -> UserInputRequest {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let SessionStatus::WaitingInput(request) = &*status.borrow()
                && request.call_id != after
            {
                return request.clone();
            }
            status.changed().await.expect("the runner keeps its status");
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no question was asked; status={:?}", *status.borrow()))
}

/// Wait for the runner to finalize.
async fn finished(status: &mut tokio::sync::watch::Receiver<SessionStatus>) -> SessionResult {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let SessionStatus::Finished(result) = &*status.borrow() {
                return result.clone();
            }
            status.changed().await.expect("the runner keeps its status");
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the session never finished; status={:?}", *status.borrow()))
}

/// The durable model-facing result of `call_id`, as the model would read it.
async fn tool_result(root: &Path, session: &str, call_id: &str) -> String {
    let entries = SessionStore::Jsonl
        .load(root, session)
        .await
        .unwrap()
        .entries;
    entries
        .iter()
        .find_map(|entry| match entry {
            crate::agent::SessionEntry::Message {
                message:
                    Message::Tool {
                        call_id: id,
                        content,
                        ..
                    },
            } if id == call_id => Some(content.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no tool result for {call_id}"))
}

/// The runner with `acp_prompt` registered, and its status receiver.
fn runner_with_acp(
    root: &Path,
    session: &str,
    tool: Box<dyn Tool>,
    user_input_allowed: bool,
) -> (
    SessionRunner,
    crate::runner::SessionHandle,
    tokio::sync::watch::Receiver<SessionStatus>,
) {
    let agent = Agent::new(Box::new(AcpCallModel { calls: 0 }), vec![tool]);
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        root.to_path_buf(),
        session.to_owned(),
        IdlePolicy::FinishWhenIdle,
    );
    let (_, _events, status) = handle.attach();
    let runner = if user_input_allowed {
        runner.with_user_input_allowed()
    } else {
        runner
    };
    (runner, handle, status)
}

#[tokio::test]
async fn a_permission_request_asks_the_user_and_sends_the_chosen_option_id() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let session = "session-ask";
    let tool = tool_with(&root, stub_config(STUB, &log, "ok"), session).await;
    let (runner, handle, mut status) = runner_with_acp(&root, session, tool, true);
    let task = runner.start(Some("go".into()));

    // Request 9001 (a `read`): ACP's own options become the question's
    // options, one-to-one, and the question names what the agent wants to do.
    let read = next_question(&mut status, "").await;
    assert!(
        read.call_id.starts_with("acp-permission-") && read.call_id.contains("-r1-"),
        "the question id is the ask's identity (nonce + ACP tool call + digest): {}",
        read.call_id
    );
    assert_eq!(read.questions.len(), 1);
    assert_eq!(read.questions[0].prompt, "read notes.md");
    assert_eq!(
        read.questions[0].options,
        vec![
            crate::runner::UserOption {
                id: "allow-1".into(),
                label: "Allow once".into(),
                description: Some("allow_once".into()),
            },
            crate::runner::UserOption {
                id: "reject-1".into(),
                label: "Reject".into(),
                description: Some("reject_once".into()),
            },
        ]
    );
    assert_eq!(
        handle.submit_prompt_with_call_id(Some(read.call_id.clone()), "allow-1".into()),
        PromptSubmission::Answered
    );

    // Request 9002 (an `execute`): the title plus the raw command.
    let run = next_question(&mut status, &read.call_id).await;
    assert!(
        run.call_id.starts_with("acp-permission-") && run.call_id.contains("-t1-"),
        "each ask has its own identity: {}",
        run.call_id
    );
    assert_ne!(read.call_id, run.call_id);
    assert_eq!(run.questions[0].prompt, "run ls: ls -la");
    assert_eq!(
        handle.submit_prompt_with_call_id(Some(run.call_id.clone()), "reject-1".into()),
        PromptSubmission::Answered
    );

    assert!(
        matches!(
            finished(&mut status).await,
            SessionResult::Completed(Some(_))
        ),
        "the turn must finish normally"
    );
    drop(task);

    // The exact frames the client sent back: the human's chosen ids.
    let frames = logged_frames(&log);
    assert_eq!(
        frames[3],
        r#"{"jsonrpc":"2.0","id":9001,"result":{"outcome":{"outcome":"selected","optionId":"allow-1"}}}"#
    );
    assert_eq!(
        frames[4],
        r#"{"jsonrpc":"2.0","id":9002,"result":{"outcome":{"outcome":"selected","optionId":"reject-1"}}}"#
    );
    let content = tool_result(&root, session, "call-acp").await;
    assert!(content.contains("alpha beta"), "{content}");
    assert!(content.contains("stopReason: end_turn"), "{content}");
    // Both requests were answered with ACP's own options, so neither is
    // reported as a denial by e-agent: the agent reports its own outcome.
    assert!(!content.contains("denied"), "{content}");
    assert!(!content.contains("read notes.md"), "{content}");
}

#[tokio::test]
async fn a_child_session_keeps_the_kind_policy_and_asks_nobody() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let session = "session-child";
    let tool = tool_with(&root, stub_config(STUB, &log, "ok"), session).await;
    // No `with_user_input_allowed`: a delegate/btw/fork child, i.e. a session
    // with no frontend. Asking would park the turn forever.
    let (runner, _handle, mut status) = runner_with_acp(&root, session, tool, false);
    let watched = status.clone();
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let recorder = {
        let seen = seen.clone();
        tokio::spawn(async move {
            let mut watched = watched;
            loop {
                let current = watched.borrow().clone();
                seen.lock().unwrap().push(format!("{current:?}"));
                if matches!(current, SessionStatus::Finished(_)) || watched.changed().await.is_err()
                {
                    break;
                }
            }
        })
    };
    let task = runner.start(Some("go".into()));

    assert!(
        matches!(
            finished(&mut status).await,
            SessionResult::Completed(Some(_))
        ),
        "a child turn must finish without asking anyone"
    );
    drop(task);
    recorder.await.unwrap();
    let seen = seen.lock().unwrap().clone();
    assert!(
        !seen.iter().any(|status| status.contains("WaitingInput")),
        "a child session must never surface a question: {seen:?}"
    );

    // Exactly today's kind policy: `read` allowed, `execute` denied, and no
    // question in between.
    let frames = logged_frames(&log);
    assert_eq!(
        frames[3],
        r#"{"jsonrpc":"2.0","id":9001,"result":{"outcome":{"outcome":"selected","optionId":"allow-1"}}}"#
    );
    assert_eq!(
        frames[4],
        r#"{"jsonrpc":"2.0","id":9002,"result":{"outcome":{"outcome":"cancelled"}}}"#
    );
    let content = tool_result(&root, session, "call-acp").await;
    assert!(content.contains("denied execute: run ls"), "{content}");
}

/// Pump the tool's question channel the way the runner does, answering with
/// `answer` (`None` = cancelled / aborted).
async fn answer_next_ask(
    commands: &mut tokio::sync::mpsc::UnboundedReceiver<crate::runner::SessionCommand>,
    answer: Option<String>,
) -> UserInputRequest {
    let ask = tokio::time::timeout(Duration::from_secs(30), commands.recv())
        .await
        .expect("the tool must ask the user")
        .expect("the session command channel stays open");
    match ask {
        crate::runner::SessionCommand::AskUser { request, mut reply } => {
            reply.send(answer);
            request
        }
        _ => panic!("expected a tool question"),
    }
}

#[tokio::test]
async fn an_answer_outside_the_offered_ids_and_an_abort_are_both_cancelled() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let session = "session-bogus";
    let mut tool = tool_with(&root, stub_config(STUB, &log, "ok"), session).await;
    let (handle, emitter, mut commands) = session_test_channel();
    emitter.allow_user_input();
    tool.set_session_handle(session, handle);
    let call = tokio::spawn(async move { tool.execute(json!({"prompt": "hi"})).await });

    // A value that names none of the offered options (the runner rejects
    // those, so this is the defensive branch): never select an invented id.
    let bogus = answer_next_ask(&mut commands, Some("selected".into())).await;
    assert!(bogus.call_id.contains("-r1-"), "{}", bogus.call_id);
    // An aborted ask (`None`) is the same outcome.
    let aborted = answer_next_ask(&mut commands, None).await;
    assert!(aborted.call_id.contains("-t1-"), "{}", aborted.call_id);
    assert_ne!(bogus.call_id, aborted.call_id);

    let output = tokio::time::timeout(Duration::from_secs(30), call)
        .await
        .expect("an unanswerable question must not hang the call")
        .unwrap()
        .expect("the stub resolves the prompt");
    let frames = logged_frames(&log);
    assert_eq!(
        frames[3],
        r#"{"jsonrpc":"2.0","id":9001,"result":{"outcome":{"outcome":"cancelled"}}}"#
    );
    assert_eq!(
        frames[4],
        r#"{"jsonrpc":"2.0","id":9002,"result":{"outcome":{"outcome":"cancelled"}}}"#
    );
    assert!(
        output
            .content
            .contains("denied read by the user: read notes.md"),
        "{}",
        output.content
    );
}

// ---------------------------------------------------------------------------
// Structural guarantees: a dead child never parks the call, an answer is bound
// to the ask it was given for, and reuse honours the negotiated capability.
// ---------------------------------------------------------------------------

/// `session/load`-free plan for a first call.
fn open_new() -> Plan {
    Plan {
        open: Open::New,
        notice: None,
    }
}

/// Wait until `ready` holds, so an assertion never races a spawned task.
async fn wait_until(mut ready: impl FnMut() -> bool, what: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}"));
}

#[tokio::test]
async fn a_child_that_exits_with_a_permission_pending_fails_the_call() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let config = stub_config(ASK_THEN_EXIT_STUB, &log, "ok");
    let (handle, emitter, _commands) = session_test_channel();
    emitter.allow_user_input();
    // A frontend exists, so the request becomes a question — and the test
    // never answers it: only the child's death can resolve the wait.
    let client = Client::connect(&config, &root, &open_new(), Some(handle))
        .await
        .expect("the stub completes the handshake");

    let prompt = client.shared.request(
        "session/prompt",
        json!({"sessionId": "sess-exit-1", "prompt": [{"type": "text", "text": "hi"}]}),
        PROMPT_TIMEOUT,
    );
    let error = tokio::time::timeout(Duration::from_secs(10), prompt)
        .await
        .expect("the prompt future must resolve when the child dies")
        .expect_err("a dead child is an error, never an answer");
    assert!(error.contains("exited during `session/prompt`"), "{error}");

    // No entry survives: the parked ask is dropped, and so is every in-flight
    // request (the reply can never arrive).
    wait_until(
        || client.shared.asking.lock().unwrap().current.is_none(),
        "the parked permission ask was never dropped",
    )
    .await;
    assert!(
        client.shared.pending.lock().await.is_empty(),
        "the in-flight request entries must be dropped"
    );
}

#[test]
fn a_permission_identity_is_unique_per_request_and_covers_the_action() {
    let read = json!({
        "toolCall": {"toolCallId": "r1", "title": "read notes.md", "kind": "read"},
        "options": [{"optionId": "allow-1", "name": "Allow once", "kind": "allow_once"}],
    });
    let execute = json!({
        "toolCall": {"toolCallId": "r1", "title": "run ls", "kind": "execute"},
        "options": [{"optionId": "allow-1", "name": "Allow once", "kind": "allow_once"}],
    });
    let first = permission_identity(Some(&read));
    let second = permission_identity(Some(&read));
    assert_ne!(
        first, second,
        "two asks with the same JSON-RPC id, tool call, and action still get \
         their own identity"
    );
    assert!(first.starts_with("acp-permission-"), "{first}");
    assert!(
        first.contains("-r1-"),
        "the identity names the tool call: {first}"
    );
    // The digest is stable for identical content and differs for anything
    // else: that is the part that covers the requested action.
    assert_eq!(action_digest(Some(&read)), action_digest(Some(&read)));
    assert_ne!(action_digest(Some(&read)), action_digest(Some(&execute)));
}

#[test]
fn an_overlapping_ask_cannot_steal_the_pending_slot() {
    let mut pending = PendingPermission::default();
    assert!(pending.claim("ask-1"), "the first ask takes the free slot");
    // A second request arrives while the first is still awaiting an answer: it
    // is refused and must leave the slot — and so the first ask's answer —
    // exactly as it was.
    assert!(
        !pending.claim("ask-2"),
        "the occupied slot is never overwritten"
    );
    assert_eq!(
        pending.current.as_deref(),
        Some("ask-1"),
        "the ask a human is looking at stays pending"
    );
    assert!(
        !pending.release("ask-2"),
        "a refused ask's answer must not release the open one"
    );
    assert_eq!(pending.current.as_deref(), Some("ask-1"));
    assert!(
        pending.release("ask-1"),
        "the first ask's own answer is used"
    );
    assert_eq!(pending.current, None);
    assert!(pending.claim("ask-3"), "the slot is free again");
}

/// A model that calls `acp_prompt` on the first provider round of EACH of two
/// turns, so one e-agent session runs the tool twice.
struct TwoRunModel {
    rounds: usize,
}

#[async_trait]
impl Model for TwoRunModel {
    async fn complete(
        &mut self,
        _: &[Message],
        _: &[ToolSpec],
        _: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        self.rounds += 1;
        let (call, prompt) = match self.rounds {
            1 => (Some("call-acp-1"), "first"),
            3 => (Some("call-acp-2"), "second"),
            _ => (None, ""),
        };
        let reply = match call {
            Some(id) => AssistantMessage {
                content: None,
                tool_calls: vec![ToolCall {
                    id: id.into(),
                    name: "acp_prompt".into(),
                    arguments: json!({"prompt": prompt}).to_string(),
                }],
                reasoning: None,
            },
            None => AssistantMessage {
                content: Some(format!("done-{}", self.rounds)),
                tool_calls: Vec::new(),
                reasoning: None,
            },
        };
        Ok((reply, None))
    }
}

/// Holds the session's background event sender: a `WaitForInput` runner whose
/// background channel has no sender left terminates the session as `Closed`.
struct KeepAlive {
    sender: Option<tokio::sync::mpsc::UnboundedSender<AgentEvent>>,
}

#[async_trait]
impl Tool for KeepAlive {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "keep_alive".into(),
            description: "test only".into(),
            parameters: json!({"type": "object"}),
        }
    }

    async fn execute(&self, _: Value) -> Result<ToolOutput, String> {
        Ok(ToolOutput::text(String::new()))
    }

    fn set_event_sender(&mut self, sender: tokio::sync::mpsc::UnboundedSender<AgentEvent>) {
        self.sender = Some(sender);
    }

    fn has_event_sender(&self) -> bool {
        self.sender.is_some()
    }
}

/// The runner with `acp_prompt` registered and an arbitrary model, keeping
/// the session alive for the next turn (`WaitForInput`).
fn runner_keeping_session(
    root: &Path,
    session: &str,
    tool: Box<dyn Tool>,
    model: Box<dyn Model>,
) -> (
    SessionRunner,
    crate::runner::SessionHandle,
    tokio::sync::watch::Receiver<SessionStatus>,
) {
    let agent = Agent::new(model, vec![tool, Box::new(KeepAlive { sender: None })]);
    let (runner, handle) = SessionRunner::new(
        agent,
        SessionStore::Jsonl,
        root.to_path_buf(),
        session.to_owned(),
        IdlePolicy::WaitForInput,
    );
    let (_, _events, status) = handle.attach();
    (runner.with_user_input_allowed(), handle, status)
}

/// Wait for the runner to be idle again (turn finished, next prompt accepted).
async fn idle(status: &mut tokio::sync::watch::Receiver<SessionStatus>) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if matches!(*status.borrow(), SessionStatus::Idle) {
                return;
            }
            status.changed().await.expect("the runner keeps its status");
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the turn never finished; status={:?}", *status.borrow()))
}

#[tokio::test]
async fn a_late_answer_to_run_one_cannot_answer_run_two() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let session = "session-stale";
    let tool = tool_with(&root, stub_config(ONE_ASK_STUB, &log, "ok"), session).await;
    let (runner, handle, mut status) =
        runner_keeping_session(&root, session, tool, Box::new(TwoRunModel { rounds: 0 }));
    let task = runner.start(Some("go".into()));

    // Run 1: a real question, answered normally.
    let first = next_question(&mut status, "").await;
    assert!(
        first.call_id.starts_with("acp-permission-"),
        "the question carries the ask's identity: {}",
        first.call_id
    );
    assert!(
        first.call_id.contains("-r1-"),
        "the identity names the ACP tool call: {}",
        first.call_id
    );
    assert_eq!(
        handle.submit_prompt_with_call_id(Some(first.call_id.clone()), "once-1".into()),
        PromptSubmission::Answered
    );
    idle(&mut status).await;

    // Run 2 reuses the same JSON-RPC id (9001) and the same tool-call id.
    handle.prompt("again");
    let second = next_question(&mut status, &first.call_id).await;
    assert_ne!(
        first.call_id, second.call_id,
        "each ask gets its own identity, even with the same JSON-RPC id"
    );
    // Run 1's answer arrives now: rejected, and run 2 keeps waiting.
    assert_eq!(
        handle.submit_prompt_with_call_id(Some(first.call_id.clone()), "always-1".into()),
        PromptSubmission::Conflict,
        "a stale identity is never accepted"
    );
    match &*status.borrow() {
        SessionStatus::WaitingInput(request) => assert_eq!(
            request.call_id, second.call_id,
            "run 2 is still the open question"
        ),
        other => panic!("run 2 must still be pending, status={other:?}"),
    }
    assert_eq!(
        handle.submit_prompt_with_call_id(Some(second.call_id.clone()), "once-1".into()),
        PromptSubmission::Answered
    );
    idle(&mut status).await;
    drop(task);

    // Both runs were answered with their OWN choice; the stale one never
    // reached either stub.
    let frames = logged_frames(&log);
    let answers: Vec<&String> = frames
        .iter()
        .filter(|frame| frame.contains(r#""outcome":"selected""#))
        .collect();
    assert_eq!(answers.len(), 2, "one answer per run: {answers:?}");
    for answer in answers {
        assert!(answer.contains(r#""optionId":"once-1""#), "{answer}");
    }
    let second_receipt = tool_result(&root, session, "call-acp-2").await;
    assert!(
        !second_receipt.contains("denied"),
        "run 2 was answered, not denied: {second_receipt}"
    );
}

#[tokio::test]
async fn a_load_that_was_not_negotiated_starts_a_new_session_and_stores_it() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let tool = tool_with(&root, stub_config(NO_LOAD_STUB, &log, "ok"), "session-a").await;

    tool.execute(json!({"prompt": "first"})).await.unwrap();
    let output = tool.execute(json!({"prompt": "second"})).await.unwrap();

    let frames = logged_frames(&log);
    assert_eq!(
        count(&frames, r#""method":"session/load""#),
        0,
        "`session/load` must not be called without `agentCapabilities.loadSession`"
    );
    assert_eq!(count(&frames, r#""method":"session/new""#), 2);
    assert!(
        output
            .content
            .starts_with("acp: stub-acp 9.9.9 · session sess-noload-2\n"),
        "{}",
        output.content
    );
    assert!(
        output.content.contains("does not support `session/load`"),
        "{}",
        output.content
    );
    // The fresh id is persisted, so the next call reuses it.
    assert_eq!(
        stored(&root, "session-a").await.as_deref(),
        Some("sess-noload-2")
    );
}

#[tokio::test]
async fn a_replayed_history_never_becomes_the_answer() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let tool = tool_with(&root, stub_config(REPLAY_STUB, &log, "ok"), "session-a").await;

    tool.execute(json!({"prompt": "first"})).await.unwrap();
    let output = tool.execute(json!({"prompt": "second"})).await.unwrap();

    let frames = logged_frames(&log);
    assert_eq!(
        count(&frames, r#""method":"session/load""#),
        1,
        "the advertised capability is used"
    );
    // The whole replay is consumed, and none of it is the answer — even
    // though it alone exceeds `TEXT_LIMIT`.
    assert!(
        output.content.contains("REAL ANSWER"),
        "the answer must survive the replay: {}",
        output.content
    );
    assert!(
        !output.content.contains("REPLAY-older-turn-"),
        "restored history must never be surfaced as the answer: {}",
        output.content
    );
    assert!(
        !output.content.contains("text truncated"),
        "the replay must not fill the answer buffer: {}",
        output.content
    );
    assert_eq!(
        stored(&root, "session-a").await.as_deref(),
        Some("sess-replay")
    );
}

#[tokio::test]
async fn a_cold_reuse_runs_in_the_cwd_of_the_current_call() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    // First call at the workspace root, then a "restart": the next tool has no
    // in-process cache, so the id can only come from the store — and the cwd
    // can only come from the call that is running now.
    let first = tool_with(&root, stub_config(CWD_STUB, &log, "ok"), "session-a").await;
    first.execute(json!({"prompt": "before"})).await.unwrap();
    assert_eq!(
        stored(&root, "session-a").await.as_deref(),
        Some("sess-cwd-1")
    );
    drop(first);
    std::fs::create_dir(root.join("sub")).unwrap();

    let restarted = tool_with(&root, stub_config(CWD_STUB, &log, "ok"), "session-a").await;
    let output = restarted
        .execute(json!({"prompt": "after", "workspace": "sub"}))
        .await
        .unwrap();

    let sub = root.join("sub");
    let frames = logged_frames(&log);
    let load = frames
        .iter()
        .find(|frame| frame.contains(r#""method":"session/load""#))
        .expect("the stored id is reused");
    assert!(
        load.contains(&format!(r#""cwd":"{}""#, sub.display())),
        "the load must run in the current call's cwd: {load}"
    );
    // The process itself was started there too (the stub logs `$(pwd)`): one
    // frame per spawned agent, and the reuse one is the call's cwd.
    assert!(
        frames
            .iter()
            .any(|frame| frame.contains(&format!(r#""stubPwd":"{}""#, sub.display()))),
        "no agents ran in {}: {}",
        sub.display(),
        frames.join("\n")
    );
    assert!(
        output.content.contains("session sess-cwd-1"),
        "{}",
        output.content
    );
}

#[tokio::test]
async fn an_allow_always_choice_is_sent_verbatim_never_downgraded() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let session = "session-allow-kinds";
    let tool = tool_with(&root, stub_config(TWO_ALLOW_STUB, &log, "ok"), session).await;
    // A REAL runner answers both questions: the submitted text goes through
    // the production conversion (`resolve_answer` → the option id), so this
    // asserts the whole path, not a hand-written answer.
    let (runner, handle, mut status) = runner_with_acp(&root, session, tool, true);
    let task = runner.start(Some("go".into()));

    // Request 9001 offers `allow_always` FIRST; the human picks it.
    let always = next_question(&mut status, "").await;
    assert!(always.call_id.contains("-r1-"), "{}", always.call_id);
    assert_eq!(
        handle.submit_prompt_with_call_id(Some(always.call_id.clone()), "always-1".into()),
        PromptSubmission::Answered
    );
    // Request 9002 offers `allow_once` first; the human picks that one.
    let once = next_question(&mut status, &always.call_id).await;
    assert!(once.call_id.contains("-r2-"), "{}", once.call_id);
    assert_eq!(
        handle.submit_prompt_with_call_id(Some(once.call_id.clone()), "once-2".into()),
        PromptSubmission::Answered
    );

    assert!(
        matches!(
            finished(&mut status).await,
            SessionResult::Completed(Some(_))
        ),
        "both questions were answered"
    );
    drop(task);
    let frames = logged_frames(&log);
    let answers: Vec<&String> = frames
        .iter()
        .filter(|frame| frame.contains(r#""outcome":"selected""#))
        .collect();
    assert_eq!(answers.len(), 2, "{answers:?}");
    assert!(
        answers[0].contains(r#""optionId":"always-1""#),
        "an `allow_always` choice must not be rewritten: {}",
        answers[0]
    );
    assert!(
        answers[1].contains(r#""optionId":"once-2""#),
        "an `allow_once` choice must not be widened: {}",
        answers[1]
    );
    let content = tool_result(&root, session, "call-acp").await;
    assert!(!content.contains("denied"), "{content}");
}

/// Two option ids that differ only by surrounding spaces both answer to `"x"`.
/// Choosing `"x"` must send back `"x"`: the id travels through the runner as
/// the validated id it was accepted as, never re-derived from the display text
/// (`"Selected option: x (Allow once)"` renders the same for both).
#[tokio::test]
async fn a_chosen_option_id_is_carried_verbatim_through_the_runner_conversion() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let session = "session-collide";
    let tool = tool_with(&root, stub_config(COLLIDING_STUB, &log, "ok"), session).await;
    let (runner, handle, mut status) = runner_with_acp(&root, session, tool, true);
    let task = runner.start(Some("go".into()));

    let ask = next_question(&mut status, "").await;
    assert_eq!(
        ask.questions[0]
            .options
            .iter()
            .map(|option| option.id.as_str())
            .collect::<Vec<_>>(),
        vec![" x ", "x"],
        "both ids reach the frontend exactly as offered"
    );
    assert_eq!(
        handle.submit_prompt_with_call_id(Some(ask.call_id.clone()), "x".into()),
        PromptSubmission::Answered
    );
    assert!(matches!(
        finished(&mut status).await,
        SessionResult::Completed(Some(_))
    ));
    drop(task);

    let frames = logged_frames(&log);
    assert!(
        frames.iter().any(|frame| frame
            == r#"{"jsonrpc":"2.0","id":9001,"result":{"outcome":{"outcome":"selected","optionId":"x"}}}"#),
        "the chosen id is sent back verbatim, never its look-alike: {frames:#?}"
    );
}

/// The merge blocker: `" x "` is the REJECT id and `"x"` is Allow once. A
/// human choosing Reject submits `" x "`, and the runner must send back the id
/// it was given — trimming it would match Allow and answer a refusal with an
/// approval. This goes through the runner (submit → `resolve_answer` → the
/// tool's ask) and asserts the actual wire frame.
#[tokio::test]
async fn rejecting_the_padded_option_sends_the_padded_id_to_the_wire() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let session = "session-reject-padded";
    let tool = tool_with(&root, stub_config(COLLIDING_STUB, &log, "ok"), session).await;
    let (runner, handle, mut status) = runner_with_acp(&root, session, tool, true);
    let task = runner.start(Some("go".into()));

    let ask = next_question(&mut status, "").await;
    assert_eq!(
        ask.questions[0]
            .options
            .iter()
            .map(|option| (option.id.as_str(), option.label.as_str()))
            .collect::<Vec<_>>(),
        vec![(" x ", "Reject"), ("x", "Allow once")],
        "the padded id is the reject option, exactly as ACP offered it"
    );
    assert_eq!(
        handle.submit_prompt_with_call_id(Some(ask.call_id.clone()), " x ".into()),
        PromptSubmission::Answered,
        "the reject id is a valid answer to this question"
    );
    assert!(matches!(
        finished(&mut status).await,
        SessionResult::Completed(Some(_))
    ));
    drop(task);

    let frames = logged_frames(&log);
    assert!(
        frames.iter().any(|frame| frame
            == r#"{"jsonrpc":"2.0","id":9001,"result":{"outcome":{"outcome":"selected","optionId":" x "}}}"#),
        "Reject must reach ACP as the padded id it offered: {frames:#?}"
    );
    assert!(
        !frames
            .iter()
            .any(|frame| frame.contains(r#""optionId":"x""#)),
        "a padded reject must never be answered as Allow once: {frames:#?}"
    );
}

/// Item 1: the runner is the one that must dismiss an ask whose requester
/// disappeared. The agent dies with the request pending (no answer is ever
/// given), so only the runner can end the wait: it must clear the question and
/// carry the tool's failure into the turn instead of parking forever.
#[tokio::test]
async fn a_runner_dismisses_an_open_ask_when_the_agent_dies() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let session = "session-die";
    let tool = tool_with(&root, stub_config(ASK_THEN_DIE_STUB, &log, "ok"), session).await;
    let (runner, _handle, mut status) = runner_with_acp(&root, session, tool, true);
    let task = runner.start(Some("go".into()));

    // The turn really reaches `WaitingInput` before the agent dies.
    let ask = next_question(&mut status, "").await;
    assert!(ask.call_id.contains("-r1-"), "{}", ask.call_id);
    // Now the child exits with the request still pending: nothing will ever
    // answer this question.
    std::fs::write(format!("{}.die", log.display()), b"").unwrap();

    assert!(
        matches!(
            finished(&mut status).await,
            SessionResult::Completed(Some(_))
        ),
        "the turn must continue with the tool's error, not park on the question"
    );
    drop(task);
    let content = tool_result(&root, session, "call-acp").await;
    assert!(
        content.contains("the agent exited during `session/prompt`"),
        "the model is told what happened: {content}"
    );
}

/// Item 2: a request that arrives while another ask is still awaiting an
/// answer must be REFUSED as overlapping — never decided by the kind policy
/// (both requests here are `read`, i.e. kinds that policy would allow) and
/// never at the cost of the open ask's own answer.
#[tokio::test]
async fn an_overlapping_ask_is_refused_never_granted_and_the_open_ask_keeps_its_answer() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let session = "session-overlap";
    let tool = tool_with(&root, stub_config(OVERLAP_STUB, &log, "ok"), session).await;
    let (runner, handle, mut status) = runner_with_acp(&root, session, tool, true);
    let task = runner.start(Some("go".into()));

    // Ask A is displayed.
    let open = next_question(&mut status, "").await;
    assert!(open.call_id.contains("-r1-"), "{}", open.call_id);
    // Ask B arrives while A is STILL open (the human has not answered yet: the
    // stub logs B's reply before A's answer can arrive) and is refused on the
    // wire. It must never become this session's question — A stays the open
    // one — and it must never be granted by the kind policy.
    wait_until(
        || log_text(&log).contains(r#""id":9002"#),
        "the overlapping request was answered right away",
    )
    .await;
    match &*status.borrow() {
        SessionStatus::WaitingInput(request) => assert_eq!(
            request.call_id, open.call_id,
            "the overlapping ask never replaces the open one"
        ),
        other => panic!("ask A must still be the open question, status={other:?}"),
    }
    assert_eq!(
        handle.submit_prompt_with_call_id(Some(open.call_id.clone()), "allow-1".into()),
        PromptSubmission::Answered
    );
    assert!(matches!(
        finished(&mut status).await,
        SessionResult::Completed(Some(_))
    ));
    drop(task);

    let frames = logged_frames(&log);
    assert!(
        frames.iter().any(|frame| frame
            == r#"{"jsonrpc":"2.0","id":9001,"result":{"outcome":{"outcome":"selected","optionId":"allow-1"}}}"#),
        "the open ask's own answer still applies: {frames:#?}"
    );
    assert!(
        frames.iter().any(|frame| frame
            == r#"{"jsonrpc":"2.0","id":9002,"result":{"outcome":{"outcome":"cancelled"}}}"#),
        "the overlapping request is refused, never auto-granted: {frames:#?}"
    );
    let content = tool_result(&root, session, "call-acp").await;
    assert!(
        content.contains("refused read without asking: another permission request is still awaiting an answer: read secrets.md"),
        "{content}"
    );
    assert!(
        !content.contains("denied"),
        "the request that WAS asked is not reported as denied: {content}"
    );
}

/// Item 6: the replay window closes with the `session/load` RESPONSE, not with
/// whatever the caller does afterwards — an update that arrives after the
/// response (but before `session/prompt`) is this call's answer.
#[tokio::test]
async fn an_update_after_the_load_response_belongs_to_the_answer() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let tool = tool_with(
        &root,
        stub_config(REPLAY_LATE_STUB, &log, "ok"),
        "session-a",
    )
    .await;

    tool.execute(json!({"prompt": "first"})).await.unwrap();
    let output = tool.execute(json!({"prompt": "second"})).await.unwrap();

    assert_eq!(
        count(&logged_frames(&log), r#""method":"session/load""#),
        1,
        "the advertised capability is used"
    );
    assert!(
        !output.content.contains("REPLAY-older-turn-"),
        "restored history is never the answer: {}",
        output.content
    );
    assert!(
        output.content.contains("EARLY LATE"),
        "an update after the load response is this call's answer: {}",
        output.content
    );
}

/// Item 2: a load that is ABANDONED (here: its deadline) must end the replay
/// region too. The response never arrives, so nothing else closes it — and the
/// `session/new` fallback's chunks would then be dropped for the rest of the
/// call. The load deadline is passed in (production uses `CALL_TIMEOUT`), so
/// the sequence runs in milliseconds instead of 30 seconds.
#[tokio::test]
async fn an_abandoned_load_ends_its_replay_region_before_the_fallback_runs() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let config = stub_config(LOAD_TIMEOUT_STUB, &log, "ok");
    let cwd = root.display().to_string();
    let mut client =
        Client::spawn(&config, &root, &open_new(), None).expect("the stub agent must start");
    client
        .handshake(&config, &root, &Open::New)
        .await
        .expect("the fresh handshake succeeds");

    // The load is abandoned by its deadline; nothing answers it.
    let load = client
        .load_session("sess-old", &cwd, Duration::from_millis(300))
        .await;
    assert!(
        load.is_err_and(|error| error.contains("session/load` timed out")),
        "the stub never answers the load, so its deadline fires"
    );

    // ... and `handshake`'s fallback for an abandoned load: a fresh session.
    let session_id = client
        .new_session(&cwd)
        .await
        .expect("the fallback session starts");
    assert_eq!(session_id, "sess-fresh-1");
    client
        .request(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "go"}]}),
            CALL_TIMEOUT,
        )
        .await
        .expect("the fallback session answers the prompt");

    let assistant = client.shared.state.lock().unwrap().assistant.clone();
    assert!(
        assistant.contains("fresh answer"),
        "the fallback session's chunks are this call's answer, never replay: {assistant:?}"
    );
}

/// Item 6: a persist failure is a failed turn. If the fresh ACP session id
/// cannot be written, the OLD durable id survives and the next call would
/// resume the wrong conversation — so the call stops before prompting.
#[cfg(unix)]
#[tokio::test]
async fn a_session_id_that_cannot_be_persisted_fails_the_call() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("frames.log");
    let root = Workspace::new(temp.path()).unwrap().root().to_path_buf();
    let session = "session-readonly";
    let tool = tool_with(&root, stub_config(STUB, &log, "ok"), session).await;
    // The metadata sidecar stays readable (the id is loaded from it) but can
    // no longer be appended to, so the fresh id cannot be persisted.
    let sidecar = root
        .join(".e-agent/sessions")
        .join(format!("{session}.meta.jsonl"));
    std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o444)).unwrap();

    let error = tool
        .execute(json!({"prompt": "review"}))
        .await
        .expect_err("an unpersistable session id must fail the call");
    assert!(
        error.contains("cannot persist ACP session id sess-new-1"),
        "{error}"
    );
    assert!(
        error.contains("the next call would resume the wrong"),
        "the error says why it is fatal: {error}"
    );
    assert_eq!(
        count(&logged_frames(&log), r#""method":"session/prompt""#),
        0,
        "the prompt is never sent with an id that could not be persisted"
    );
}
