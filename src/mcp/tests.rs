use super::*;

/// A minimal fake MCP server as a bash script: reads NDJSON lines from
/// stdin, responds to initialize / tools/list / tools/call.
const FAKE_SERVER: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  case "$line" in
    *initialize*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fake"},"instructions":"fake instructions"}}\n' "$id"
      ;;
    *tools/list*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"echo back","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}}\n' "$id"
      ;;
    *tools/call*)
      text=$(printf '%s' "$line" | grep -o '"text":"[^"]*"' | head -1 | cut -d'"' -f4)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"you said: %s"}]}}\n' "$id" "$text"
      ;;
  esac
done
"#;

/// A fake server that sleeps `$SLEEP` seconds before answering initialize
/// (0 by default), or never answers at all when `$HANG` is set.
const SLOW_SERVER: &str = r#"
while IFS= read -r line; do
  if [ -n "$HANG" ]; then
    continue
  fi
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  case "$line" in
    *'"method":"initialize"'*)
      sleep "${SLEEP:-0}"
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"slow"},"instructions":"slow instructions"}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"tool","description":"a tool","inputSchema":{"type":"object"}}]}}\n' "$id"
      ;;
  esac
done
"#;

fn fake_server_config() -> McpServerConfig {
    McpServerConfig {
        command: vec!["/bin/bash".into(), "-c".into(), FAKE_SERVER.into()],
        env: HashMap::new(),
        cwd: None,
        enabled: true,
    }
}

fn slow_server_config(env: HashMap<String, String>) -> McpServerConfig {
    McpServerConfig {
        command: vec!["/bin/bash".into(), "-c".into(), SLOW_SERVER.into()],
        env,
        cwd: None,
        enabled: true,
    }
}

#[tokio::test]
async fn handshake_lists_tools_and_calls_them() {
    let temp = tempfile::tempdir().unwrap();
    let (server, instructions) = McpServer::connect("fake", &fake_server_config(), temp.path())
        .await
        .unwrap();
    assert_eq!(instructions.as_deref(), Some("fake instructions"));

    let list = server.list_tools().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["name"], "echo");

    let output = server
        .call_tool("echo", json!({"text": "hello"}))
        .await
        .unwrap();
    assert_eq!(output, "you said: hello");
}

#[tokio::test]
async fn connect_all_exposes_prefixed_tools() {
    let temp = tempfile::tempdir().unwrap();
    let servers = HashMap::from([("fake".to_owned(), fake_server_config())]);
    let (tools, instructions) = connect_all(servers, temp.path()).await;
    assert_eq!(instructions.len(), 1);
    assert!(instructions[0].contains("fake instructions"));
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].spec().name, "fake_echo");
    let output = tools[0].execute(json!({"text": "hi"})).await.unwrap();
    assert_eq!(output, "you said: hi");
}

#[tokio::test]
async fn connect_all_connects_servers_in_parallel() {
    let temp = tempfile::tempdir().unwrap();
    let slow = slow_server_config(HashMap::from([("SLEEP".to_owned(), "2".to_owned())]));
    let servers = HashMap::from([
        ("slow_a".to_owned(), slow.clone()),
        ("slow_b".to_owned(), slow),
    ]);
    let started = std::time::Instant::now();
    let (tools, instructions) = connect_all(servers, temp.path()).await;
    let elapsed = started.elapsed();
    // Both servers contribute one tool each; a serial implementation would
    // take ~4s and a parallel one ~2s, so bound with generous slack.
    assert_eq!(tools.len(), 2, "both servers should contribute one tool");
    let mut names: Vec<String> = tools.iter().map(|t| t.spec().name.clone()).collect();
    names.sort();
    assert_eq!(
        names,
        vec!["slow_a_tool".to_owned(), "slow_b_tool".to_owned()]
    );
    assert_eq!(instructions.len(), 2);
    assert!(
        elapsed < Duration::from_secs(4),
        "connect_all took {elapsed:?}; expected parallel (~2s), not serial (~4s)"
    );
}

#[tokio::test]
async fn connect_all_timeout_keeps_partial_results() {
    let temp = tempfile::tempdir().unwrap();
    let fast = fake_server_config();
    let hanging = slow_server_config(HashMap::from([("HANG".to_owned(), "1".to_owned())]));
    let servers = HashMap::from([("fast".to_owned(), fast), ("hanging".to_owned(), hanging)]);
    let started = std::time::Instant::now();
    let (tools, instructions) =
        connect_all_with_timeout(servers, temp.path(), Duration::from_millis(500)).await;
    let elapsed = started.elapsed();
    // The fast server finished before the deadline; the hanging one did not.
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].spec().name, "fast_echo");
    assert_eq!(instructions.len(), 1);
    assert!(instructions[0].contains("fake instructions"));
    // Returned at the deadline, not after the hanging server's 30s CALL_TIMEOUT.
    assert!(
        elapsed < Duration::from_secs(5),
        "connect_all did not honor the timeout: took {elapsed:?}"
    );
}

#[tokio::test]
async fn connect_all_timeout_returns_empty_when_everything_hangs() {
    let temp = tempfile::tempdir().unwrap();
    let hanging = slow_server_config(HashMap::from([("HANG".to_owned(), "1".to_owned())]));
    let servers = HashMap::from([("hanging".to_owned(), hanging)]);
    let started = std::time::Instant::now();
    let (tools, instructions) =
        connect_all_with_timeout(servers, temp.path(), Duration::from_millis(300)).await;
    let elapsed = started.elapsed();
    assert!(tools.is_empty());
    assert!(instructions.is_empty());
    assert!(
        elapsed >= Duration::from_millis(200),
        "returned too early: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "connect_all did not honor the timeout: took {elapsed:?}"
    );
}
