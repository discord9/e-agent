//! A piped `--repl` must answer a blocking question that arrives while it is
//! waiting for the turn to finish, and it must submit the chosen option id
//! byte-for-byte.
//!
//! Both defects this guards are invisible to a direct `repl_answer` call
//! (`src/main_tests.rs`), because neither is about that function alone:
//!
//! 1. the REPL used to dispatch `WaitingInput` only ONCE, before it entered the
//!    idle wait. Sending a prompt first wakes it on `Busy`, so the question
//!    raised later woke the wait loop without leaving it: the answer was never
//!    read, even when it was already in the pipe;
//! 2. `repl_answer` stripped a trailing `\r` even when no `\n` preceded it, so
//!    a final answer at EOF (`"x\r"`, no newline) was repaired into `"x"` —
//!    the OTHER valid option id, i.e. Allow instead of Reject.
//!
//! This run therefore drives the real binary through its real read loop:
//! piped stdin carries one prompt line and then `"x\r"` with no trailing
//! newline; the mock provider holds its first reply, so the REPL is inside its
//! idle wait when the question is raised; and the stub ACP agent offers two
//! options whose ids are `"x\r"` (Reject) and `"x"` (Allow once). Only the
//! `optionId` ACP receives back proves what the REPL actually did.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// How long the mock provider holds its first reply: long enough that the REPL
/// is already waiting for the runner (it just woke on `Busy`) when the ACP
/// question is raised, which is exactly the window defect 1 needed.
const BUSY_HOLD: Duration = Duration::from_millis(1200);
/// A short hold before the closing reply, so the stub can log the answer frame
/// before the turn ends and the REPL exits.
const AFTER_ANSWER: Duration = Duration::from_millis(300);
/// Generous deadline for the answer to come back through the whole pipe.
const DEADLINE: Duration = Duration::from_secs(30);

/// A stub ACP agent (`$1` = its frame log): speaks newline-delimited JSON-RPC
/// on stdin/stdout. On `session/prompt` it asks for permission with two options
/// whose ids differ only by the carriage return — `"x\r"` (Reject) and `"x"`
/// (Allow once) — waits for the ONE answer, and logs the frame it received.
const ACP_STUB: &str = r#"
log="$1"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  printf '%s\n' "$line" >> "$log"
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{},"agentInfo":{"name":"stub-acp","version":"9.9.9"},"authMethods":[]}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"sess-repl-1"}}\n' "$id"
      ;;
    *'"method":"session/prompt"'*)
      printf '{"jsonrpc":"2.0","id":9001,"method":"session/request_permission","params":{"sessionId":"sess-repl-1","toolCall":{"toolCallId":"r1","title":"read notes.md","kind":"read"},"options":[{"optionId":"x\\r","name":"Reject","kind":"reject_once"},{"optionId":"x","name":"Allow once","kind":"allow_once"}]}}\n'
      IFS= read -r answer
      printf '%s\n' "$answer" >> "$log"
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
  esac
done
"#;

/// A stub ACP agent (`$1` = its frame log) whose every `session/prompt` asks
/// for permission with the SAME option ids (`allow-1` / `reject-1`), but names
/// its tool call `r1` for the first ask and `r2` for the second. The log
/// therefore records, in order, which request got which `optionId` back.
const ACP_STUB_TWO_QUESTIONS: &str = r#"
log="$1"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  printf '%s\n' "$line" >> "$log"
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{},"agentInfo":{"name":"stub-acp","version":"9.9.9"},"authMethods":[]}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"sess-repl-two"}}\n' "$id"
      ;;
    *'"method":"session/prompt"'*)
      seen=$(grep -c 'session/prompt' "$log")
      if [ "$seen" -eq 1 ]; then
        call='{"toolCallId":"r1","title":"first request","kind":"read"}'
      else
        call='{"toolCallId":"r2","title":"second request","kind":"read"}'
      fi
      perm=$(printf '{"jsonrpc":"2.0","id":9001,"method":"session/request_permission","params":{"sessionId":"sess-repl-two","toolCall":%s,"options":[{"optionId":"allow-1","name":"Allow once","kind":"allow_once"},{"optionId":"reject-1","name":"Reject","kind":"reject_once"}]}}' "$call")
      printf '%s\n' "$perm" >> "$log"
      printf '%s\n' "$perm"
      IFS= read -r answer
      printf '%s\n' "$answer" >> "$log"
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
  esac
done
"#;

/// Read one HTTP request off `stream` and return its body.
fn read_request(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let header_end = loop {
        if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        let mut chunk = [0u8; 4096];
        let count = stream.read(&mut chunk).expect("read the provider request");
        assert!(count > 0, "the provider request ended before its headers");
        request.extend_from_slice(&chunk[..count]);
    };
    let headers = String::from_utf8_lossy(&request[..header_end]).to_string();
    let length: usize = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().expect("a numeric content length"))
        })
        .unwrap_or(0);
    while request.len() < header_end + length {
        let mut chunk = [0u8; 4096];
        let count = stream.read(&mut chunk).expect("read the provider body");
        assert!(count > 0, "the provider request ended before its body");
        request.extend_from_slice(&chunk[..count]);
    }
    String::from_utf8_lossy(&request[header_end..header_end + length]).to_string()
}

/// Answer one streamed `chat/completions` call with `deltas`.
fn respond_sse(stream: &mut TcpStream, deltas: &[Value]) {
    let mut body = String::new();
    for delta in deltas {
        body.push_str(&format!(
            "data: {}\n\n",
            serde_json::to_string(delta).expect("serialize one SSE chunk")
        ));
    }
    body.push_str("data: [DONE]\n\n");
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(header.as_bytes())
        .expect("write the provider headers");
    stream
        .write_all(body.as_bytes())
        .expect("write the provider body");
    stream.flush().expect("flush the provider body");
}

fn chunk(delta: Value, finish_reason: Option<&str>) -> Value {
    json!({
        "id": "repl-e2e",
        "object": "chat.completion.chunk",
        "model": "repl-e2e",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
    })
}

/// The mock OpenAI-compatible provider: first call → one `acp_prompt` tool
/// call (held back for [`BUSY_HOLD`]), second call → the closing answer.
fn spawn_mock_provider() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the mock provider");
    let address = listener.local_addr().expect("read the mock provider addr");
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept the first call");
        let _ = read_request(&mut stream);
        std::thread::sleep(BUSY_HOLD);
        respond_sse(
            &mut stream,
            &[
                chunk(
                    json!({"tool_calls": [{
                        "index": 0,
                        "id": "call-acp-1",
                        "type": "function",
                        "function": {
                            "name": "acp_prompt",
                            "arguments": "{\"prompt\":\"review notes.md\"}",
                        },
                    }]}),
                    None,
                ),
                chunk(json!({}), Some("tool_calls")),
            ],
        );
        drop(stream);

        let (mut stream, _) = listener.accept().expect("accept the second call");
        let _ = read_request(&mut stream);
        std::thread::sleep(AFTER_ANSWER);
        respond_sse(
            &mut stream,
            &[
                chunk(json!({"content": "done"}), None),
                chunk(json!({}), Some("stop")),
            ],
        );
    });
    format!("http://{address}/v1")
}

/// The mock provider for the two-question run: call 1 answers with the first
/// `acp_prompt` tool call (held back for [`BUSY_HOLD`] so the REPL is already
/// waiting when its question arrives), call 2 with the second, calls 3 and 4
/// with a closing answer. Every request body is recorded so the caller can
/// check which line arrived as an ordinary prompt.
fn spawn_two_question_provider(requests: Arc<Mutex<Vec<Value>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the mock provider");
    let address = listener.local_addr().expect("read the mock provider addr");
    std::thread::spawn(move || {
        for call in 0..4 {
            let (mut stream, _) = listener.accept().expect("accept a provider call");
            let body = read_request(&mut stream);
            requests
                .lock()
                .unwrap()
                .push(serde_json::from_str(&body).expect("parse the provider request"));
            if call == 0 {
                std::thread::sleep(BUSY_HOLD);
            }
            let deltas = match call {
                0 => vec![
                    chunk(
                        json!({"tool_calls": [{
                            "index": 0,
                            "id": "call-acp-1",
                            "type": "function",
                            "function": {
                                "name": "acp_prompt",
                                "arguments": "{\"prompt\":\"review notes.md\"}",
                            },
                        }]}),
                        None,
                    ),
                    chunk(json!({}), Some("tool_calls")),
                ],
                1 => vec![
                    chunk(
                        json!({"tool_calls": [{
                            "index": 0,
                            "id": "call-acp-2",
                            "type": "function",
                            "function": {
                                "name": "acp_prompt",
                                "arguments": "{\"prompt\":\"review more notes.md\"}",
                            },
                        }]}),
                        None,
                    ),
                    chunk(json!({}), Some("tool_calls")),
                ],
                _ => vec![
                    chunk(json!({"content": "done"}), None),
                    chunk(json!({}), Some("stop")),
                ],
            };
            respond_sse(&mut stream, &deltas);
            drop(stream);
        }
    });
    format!("http://{address}/v1")
}

/// The `optionId` the stub logged for the permission request, if it arrived.
fn chosen_option_id(frames: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(frames).ok()?;
    let mut chosen = None;
    for line in text.lines() {
        let Ok(frame) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if frame["id"] != json!(9001) {
            continue;
        }
        chosen = Some(frame["result"]["outcome"]["optionId"].clone());
    }
    chosen
}

/// Pair every `session/request_permission` (by its `toolCallId`) with the
/// `optionId` e-agent sent back for it, in frame order. `None` means the
/// request is still unanswered.
fn permission_answers(frames: &Path) -> Vec<(String, Option<Value>)> {
    let text = std::fs::read_to_string(frames).unwrap_or_default();
    let mut answers: Vec<(String, Option<Value>)> = Vec::new();
    for line in text.lines() {
        let Ok(frame) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if frame["method"] == json!("session/request_permission") {
            answers.push((
                frame["params"]["toolCall"]["toolCallId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                None,
            ));
        } else if frame["id"] == json!(9001)
            && let Some(last) = answers.last_mut()
        {
            last.1 = Some(frame["result"]["outcome"]["optionId"].clone());
        }
    }
    answers
}

/// The `role: "user"` message texts in one provider request body.
fn user_prompts(request: &Value) -> Vec<String> {
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == json!("user"))
        .filter_map(|message| message["content"].as_str().map(str::to_owned))
        .collect()
}

/// Spawn the real `--repl` binary with piped stdin under an isolated
/// HOME/config, using `stub` as the `[acp]` agent. The returned temp root must
/// outlive the child: it holds the workspace, config, and both logs.
fn launch_piped_repl(stub: &str, base_url: &str) -> (tempfile::TempDir, PathBuf, PathBuf, Child) {
    let root = tempfile::tempdir().expect("temp root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let config_home = root.path().join("config");
    std::fs::create_dir_all(config_home.join("e-agent")).expect("config root");
    for name in ["home", "state", "cache"] {
        std::fs::create_dir(root.path().join(name)).expect("isolated dir");
    }
    let stub_path = root.path().join("acp-stub.sh");
    std::fs::write(&stub_path, stub).expect("write the ACP stub");
    let frames = root.path().join("acp-frames.log");
    let child_log = root.path().join("child.log");

    std::fs::write(
        config_home.join("e-agent/config.toml"),
        format!(
            "default = \"mock/repl-e2e\"\n\
             [providers.mock]\n\
             base_url = \"{base_url}\"\n\
             api_key_env = \"REPL_E2E_KEY\"\n\
             [models.\"mock/repl-e2e\"]\n\
             model = \"repl-e2e\"\n\
             [session]\n\
             backend = \"jsonl\"\n\
             [acp]\n\
             binary = \"/bin/bash\"\n\
             args = [{stub:?}, {frames:?}]\n",
            stub = stub_path.display().to_string(),
            frames = frames.display().to_string(),
        ),
    )
    .expect("write the isolated config");

    let log = std::fs::File::create(&child_log).expect("child log");
    let child = Command::new(env!("CARGO_BIN_EXE_e-agent"))
        .arg("--repl")
        .arg("--workspace")
        .arg(&workspace)
        .env("HOME", root.path().join("home"))
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("REPL_E2E_KEY", "isolated-dummy-not-a-credential")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .stdin(Stdio::piped())
        .stdout(Stdio::from(log.try_clone().expect("clone the child log")))
        .stderr(Stdio::from(log))
        .spawn()
        .expect("spawn the piped --repl");
    (root, frames, child_log, child)
}

#[test]
fn a_piped_repl_answers_after_busy_and_keeps_the_carriage_return() {
    let base_url = spawn_mock_provider();
    let (_root, frames, child_log, mut child) = launch_piped_repl(ACP_STUB, &base_url);

    // One prompt line, then the Reject answer at EOF WITHOUT a trailing
    // newline: `read_line` hands back `"x\r"`, and the id must survive as is.
    child
        .stdin
        .as_mut()
        .expect("piped stdin")
        .write_all(b"go\nx\r")
        .expect("write the piped script");
    drop(child.stdin.take());

    let deadline = Instant::now() + DEADLINE;
    let chosen = loop {
        if let Some(chosen) = chosen_option_id(&frames) {
            break Some(chosen);
        }
        let exited = child.try_wait().expect("wait for the child").is_some();
        if exited || Instant::now() >= deadline {
            break chosen_option_id(&frames);
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let chosen = match chosen {
        Some(chosen) => chosen,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "the piped --repl never answered the question, so ACP received \
                 nothing at all (the question was raised after Busy and the REPL \
                 only dispatched WaitingInput before its idle wait)\n\
                 --- ACP frames ---\n{}\n--- child log ---\n{}",
                std::fs::read_to_string(&frames).unwrap_or_default(),
                std::fs::read_to_string(&child_log).unwrap_or_default(),
            );
        }
    };
    // Both ids are valid options, so only the exact bytes prove which one was
    // chosen: `"x"` is Allow once, `"x\r"` is Reject.
    assert_eq!(
        chosen,
        json!("x\r"),
        "ACP must receive the submitted id verbatim, not the repaired `x`\n\
         --- ACP frames ---\n{}\n--- child log ---\n{}",
        std::fs::read_to_string(&frames).unwrap_or_default(),
        std::fs::read_to_string(&child_log).unwrap_or_default(),
    );

    let done = Instant::now() + Duration::from_secs(10);
    while Instant::now() < done && child.try_wait().expect("wait for the child").is_none() {
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The reported wrong-ID case: two questions reuse the same option ids, and
/// the pipe is meant to answer A, answer B, then prompt again. When the REPL
/// dispatched `WaitingInput` with a plain `borrow`, the value it showed was
/// never marked seen, so the `changed()` after A's answer returned for A
/// itself: the loop offered the stale question the line meant for B
/// (`Conflict`), B received the line meant as the ordinary prompt, and the
/// prompt never reached the provider. "Every submitted id was valid" cannot
/// tell that apart from success — the stub's per-request `optionId` and the
/// provider's last request can.
#[test]
fn two_questions_reusing_the_same_ids_route_each_line_to_its_own_ask() {
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let base_url = spawn_two_question_provider(Arc::clone(&requests));
    let (_root, frames, child_log, mut child) =
        launch_piped_repl(ACP_STUB_TWO_QUESTIONS, &base_url);

    child
        .stdin
        .as_mut()
        .expect("piped stdin")
        .write_all(b"go\nallow-1\nreject-1\nallow-1\n")
        .expect("write the piped script");
    drop(child.stdin.take());

    let deadline = Instant::now() + DEADLINE;
    let answered = loop {
        let answers = permission_answers(&frames);
        if answers.len() == 2
            && answers.iter().all(|(_, option)| option.is_some())
            && requests.lock().unwrap().len() >= 4
        {
            break answers;
        }
        if child.try_wait().expect("wait for the child").is_some() || Instant::now() >= deadline {
            break permission_answers(&frames);
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // The fourth provider call answers the ordinary prompt; after it the turn
    // ends and the REPL reads EOF, so let the child finish (or kill a hung
    // one) before snapshotting what arrived.
    let done = Instant::now() + Duration::from_secs(10);
    while Instant::now() < done && child.try_wait().expect("wait for the child").is_none() {
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();

    let sent: Vec<Vec<String>> = requests.lock().unwrap().iter().map(user_prompts).collect();
    let context = format!(
        "--- ACP frames ---\n{}\n--- child log ---\n{}\n--- provider requests ---\n{}",
        std::fs::read_to_string(&frames).unwrap_or_default(),
        std::fs::read_to_string(&child_log).unwrap_or_default(),
        sent.iter()
            .enumerate()
            .map(|(index, prompts)| format!("call {}: {prompts:?}\n", index + 1))
            .collect::<String>(),
    );

    assert_eq!(
        answered,
        vec![
            ("r1".to_owned(), Some(json!("allow-1"))),
            ("r2".to_owned(), Some(json!("reject-1"))),
        ],
        "each answer line must reach the question it was written for: the \
         first line (`allow-1`) answers r1 and the second (`reject-1`) answers \
         r2, even though both questions offer both ids\n{context}"
    );

    assert_eq!(
        sent.len(),
        4,
        "the pipe has exactly one ordinary prompt after the two answers, so \
         the provider must see exactly four calls (two tool-call rounds, the \
         closing answer, and the ordinary prompt)\n{context}"
    );
    assert_eq!(
        sent[3],
        vec!["go".to_owned(), "allow-1".to_owned()],
        "the last pipe line (`allow-1`) must arrive as the ordinary prompt of \
         the fourth call, not be consumed as an answer\n{context}"
    );
}
