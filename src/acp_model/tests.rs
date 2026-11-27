//! The ACP `Model` is driven against a stub agent on the other end of
//! in-memory pipes — the same fake-transport idiom `src/codex/tests.rs` uses for
//! its local HTTP stub.

use super::*;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

/// A stub ACP agent: a JSON-RPC peer over a pipe pair.
struct Stub {
    reader: BufReader<DuplexStream>,
    writer: DuplexStream,
    /// Every request the client sent, in arrival order.
    sent: Vec<Value>,
}

/// A model wired to a fresh stub agent.
fn pipe_model() -> (AcpModel, Stub) {
    let (client_write, stub_read) = tokio::io::duplex(1 << 16);
    let (stub_write, client_read) = tokio::io::duplex(1 << 16);
    let stub = Stub {
        reader: BufReader::new(stub_read),
        writer: stub_write,
        sent: Vec::new(),
    };
    let client = AcpClient::from_io(None, client_read, client_write);
    (AcpModel::with_client(client, std::env::temp_dir()), stub)
}

impl Stub {
    async fn line(&mut self) -> Option<Value> {
        let mut line = String::new();
        if self.reader.read_line(&mut line).await.unwrap() == 0 {
            return None;
        }
        Some(serde_json::from_str(&line).expect("client wrote JSON"))
    }

    /// Read the next client message and assert it is a `method` request.
    async fn request(&mut self, method: &str) -> Value {
        let message = self
            .line()
            .await
            .unwrap_or_else(|| panic!("client closed the pipe before `{method}`"));
        assert_eq!(message["method"], method, "unexpected message: {message}");
        self.sent.push(message.clone());
        message
    }

    async fn send(&mut self, message: Value) {
        let mut line = serde_json::to_string(&message).unwrap();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn reply(&mut self, request: &Value, result: Value) {
        self.send(serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"].clone(),
            "result": result,
        }))
        .await;
    }

    async fn chunk(&mut self, session_id: &str, text: &str) {
        self.send(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": text},
                },
            },
        }))
        .await;
    }

    /// `initialize` + `session/new`, returning the created session id.
    async fn handshake(&mut self) -> String {
        let initialize = self.request("initialize").await;
        assert_eq!(initialize["params"]["protocolVersion"], 1);
        self.reply(
            &initialize,
            serde_json::json!({"protocolVersion": 1, "agentCapabilities": {}}),
        )
        .await;
        let new = self.request("session/new").await;
        assert_eq!(new["params"]["mcpServers"], serde_json::json!([]));
        self.reply(&new, serde_json::json!({"sessionId": "sess-1"}))
            .await;
        "sess-1".to_owned()
    }
}

fn user(text: &str) -> Message {
    Message::User {
        content: text.to_owned(),
        images: vec![],
    }
}

fn assistant(text: &str) -> Message {
    Message::Assistant(AssistantMessage {
        content: Some(text.to_owned()),
        tool_calls: vec![],
        reasoning: None,
    })
}

#[tokio::test]
async fn two_turns_initialize_once_and_send_only_the_newest_user_text() {
    let (mut model, mut stub) = pipe_model();
    let agent = tokio::spawn(async move {
        let session = stub.handshake().await;
        let first = stub.request("session/prompt").await;
        assert_eq!(first["params"]["sessionId"], session);
        assert_eq!(
            first["params"]["prompt"],
            serde_json::json!([{"type": "text", "text": "first question"}])
        );
        stub.reply(
            &first,
            serde_json::json!({
                "stopReason": "end_turn",
                "usage": {"inputTokens": 7, "outputTokens": 3},
            }),
        )
        .await;
        let second = stub.request("session/prompt").await;
        assert_eq!(second["params"]["sessionId"], session);
        // ACP is stateful: history is NOT replayed, only the newest user text.
        assert_eq!(
            second["params"]["prompt"],
            serde_json::json!([{"type": "text", "text": "second question"}])
        );
        stub.reply(&second, serde_json::json!({"stopReason": "end_turn"}))
            .await;
        stub.sent
    });

    let (first, usage) = model
        .complete(&[user("first question")], &[], None)
        .await
        .unwrap();
    assert_eq!(first.content.as_deref(), Some(""));
    assert_eq!(
        usage,
        Some(Usage {
            input_tokens: 7,
            output_tokens: 3,
            cache_hit_tokens: None,
            cache_miss_tokens: None,
            reasoning_tokens: None,
            finish_reason: Some("end_turn".into()),
        })
    );

    let history = [
        user("first question"),
        assistant("first answer"),
        user("second question"),
    ];
    let (second, usage) = model.complete(&history, &[], None).await.unwrap();
    assert_eq!(second.content.as_deref(), Some(""));
    assert_eq!(usage, None);

    let sent = agent.await.unwrap();
    let methods: Vec<&str> = sent
        .iter()
        .map(|message| message["method"].as_str().unwrap())
        .collect();
    assert_eq!(
        methods,
        [
            "initialize",
            "session/new",
            "session/prompt",
            "session/prompt"
        ]
    );
}

#[tokio::test]
async fn chunks_stream_in_order_and_accumulate_into_the_message() {
    let (mut model, mut stub) = pipe_model();
    let agent = tokio::spawn(async move {
        let session = stub.handshake().await;
        let prompt = stub.request("session/prompt").await;
        for text in ["Hello", ", ", "world"] {
            stub.chunk(&session, text).await;
        }
        stub.reply(&prompt, serde_json::json!({"stopReason": "end_turn"}))
            .await;
    });

    let mut deltas = Vec::new();
    let (message, _) = model
        .complete(
            &[user("say hello")],
            &[],
            Some(&mut |kind, text| deltas.push((kind, text.to_owned()))),
        )
        .await
        .unwrap();
    agent.await.unwrap();

    assert_eq!(
        deltas,
        [
            (ModelDeltaKind::Content, "Hello".to_owned()),
            (ModelDeltaKind::Content, ", ".to_owned()),
            (ModelDeltaKind::Content, "world".to_owned()),
        ]
    );
    assert_eq!(message.content.as_deref(), Some("Hello, world"));
    assert!(message.tool_calls.is_empty());
}

#[tokio::test]
async fn permission_requests_are_declined_and_the_turn_still_finishes() {
    let (mut model, mut stub) = pipe_model();
    let agent = tokio::spawn(async move {
        let session = stub.handshake().await;
        let prompt = stub.request("session/prompt").await;
        stub.send(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "session/request_permission",
            "params": {
                "sessionId": session,
                "toolCall": {"toolCallId": "call-1", "title": "write the file"},
                "options": [
                    {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                    {"optionId": "reject", "name": "Reject", "kind": "reject_once"},
                ],
            },
        }))
        .await;
        let answer = stub
            .line()
            .await
            .expect("the client must answer the permission request");
        assert_eq!(answer["id"], 99);
        assert_eq!(
            answer["result"],
            serde_json::json!({"outcome": {"outcome": "cancelled"}})
        );
        stub.chunk(&session, "done without permission").await;
        stub.reply(&prompt, serde_json::json!({"stopReason": "end_turn"}))
            .await;
    });

    let (message, _) = model
        .complete(&[user("edit the file")], &[], None)
        .await
        .unwrap();
    agent.await.unwrap();
    assert_eq!(message.content.as_deref(), Some("done without permission"));
}

#[tokio::test]
async fn child_exit_mid_turn_fails_the_turn_and_ends_the_session() {
    let (mut model, mut stub) = pipe_model();
    tokio::spawn(async move {
        let session = stub.handshake().await;
        stub.request("session/prompt").await;
        stub.chunk(&session, "partial answer").await;
        // The agent dies mid-turn: its stdout closes with no prompt reply.
        drop(stub);
    });

    let error = model
        .complete(&[user("hello")], &[], None)
        .await
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("child died"), "unexpected error: {text}");

    // A dead ACP session is never silently replaced by a fresh agent.
    let error = model
        .complete(&[user("hello again")], &[], None)
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("after an earlier failure"),
        "unexpected error: {error:#}"
    );
}

#[tokio::test]
async fn initialize_errors_name_the_agent_cause() {
    let (mut model, mut stub) = pipe_model();
    tokio::spawn(async move {
        let initialize = stub.request("initialize").await;
        stub.send(serde_json::json!({
            "jsonrpc": "2.0",
            "id": initialize["id"].clone(),
            "error": {"code": -32603, "message": "no credentials configured"},
        }))
        .await;
    });

    let error = model
        .complete(&[user("hello")], &[], None)
        .await
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("initialize"), "unexpected error: {text}");
    assert!(
        text.contains("no credentials configured"),
        "unexpected error: {text}"
    );
}

#[tokio::test]
async fn initialize_rejects_a_foreign_protocol_version() {
    let (mut model, mut stub) = pipe_model();
    tokio::spawn(async move {
        let initialize = stub.request("initialize").await;
        stub.reply(&initialize, serde_json::json!({"protocolVersion": 2}))
            .await;
    });

    let error = model
        .complete(&[user("hello")], &[], None)
        .await
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("protocol version 2"),
        "unexpected error: {text}"
    );
}
