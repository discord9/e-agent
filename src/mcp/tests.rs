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

fn fake_server_config() -> McpServerConfig {
    McpServerConfig {
        command: vec!["/bin/bash".into(), "-c".into(), FAKE_SERVER.into()],
        env: HashMap::new(),
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
