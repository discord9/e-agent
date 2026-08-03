use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::*;
use crate::agent::{Agent, Tool};

#[test]
fn converts_internal_messages_and_function_schemas() {
    let tools = [ToolSpec {
        name: "read_file".into(),
        description: "read".into(),
        parameters: json!({"type":"object"}),
    }];
    let request = ChatRequest::from_internal(
        "test-model",
        None,
        false,
        &[
            Message::Assistant(AssistantMessage {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "read_file".into(),
                    arguments: r#"{"path":"a.txt"}"#.into(),
                }],
                reasoning: None,
            }),
            Message::Tool {
                call_id: "call-1".into(),
                name: "read_file".into(),
                content: "not found".into(),
                is_error: true,
                synthetic: false,
            },
        ],
        &tools,
        None,
    );
    let value = serde_json::to_value(request).unwrap();
    assert_eq!(value["tools"][0]["type"], "function");
    assert_eq!(value["tools"][0]["function"]["name"], "read_file");
    assert_eq!(
        value["messages"][0]["tool_calls"][0]["function"]["arguments"],
        r#"{"path":"a.txt"}"#
    );
    assert_eq!(value["messages"][1]["content"], "ERROR: not found");
    assert_eq!(value["messages"][1]["tool_call_id"], "call-1");
}

#[test]
fn omits_empty_tool_calls_on_plain_assistant_messages() {
    let request = ChatRequest::from_internal(
        "test-model",
        None,
        false,
        &[Message::Assistant(AssistantMessage {
            content: Some("done".into()),
            tool_calls: vec![],
            reasoning: None,
        })],
        &[],
        None,
    );
    let value = serde_json::to_value(request).unwrap();
    assert_eq!(value["messages"][0]["content"], "done");
    assert!(value["messages"][0].get("tool_calls").is_none());
}

#[test]
fn serializes_reasoning_effort_at_the_top_level() {
    let request = ChatRequest::from_internal("test-model", Some("max"), false, &[], &[], None);
    let value = serde_json::to_value(request).unwrap();
    assert_eq!(value["reasoning_effort"], "max");
}

#[test]
fn omits_reasoning_effort_when_unset() {
    let request = ChatRequest::from_internal("test-model", None, false, &[], &[], None);
    let value = serde_json::to_value(request).unwrap();
    assert!(value.get("reasoning_effort").is_none());
}

#[test]
fn serializes_thinking_switch_when_enabled() {
    let request = ChatRequest::from_internal("test-model", Some("max"), true, &[], &[], None);
    let value = serde_json::to_value(request).unwrap();
    assert_eq!(value["thinking"]["type"], "enabled");
}

#[test]
fn omits_thinking_switch_when_disabled_or_without_effort() {
    // `thinking = false` must not emit the field even with an effort set.
    let disabled = serde_json::to_value(ChatRequest::from_internal(
        "test-model",
        Some("max"),
        false,
        &[],
        &[],
        None,
    ))
    .unwrap();
    assert!(disabled.get("thinking").is_none());

    // `thinking = true` alone (no reasoning_effort) must not emit it either.
    let no_effort = serde_json::to_value(ChatRequest::from_internal(
        "test-model",
        None,
        true,
        &[],
        &[],
        None,
    ))
    .unwrap();
    assert!(no_effort.get("thinking").is_none());
}

async fn read_request(stream: &mut TcpStream) -> serde_json::Value {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0; 1024];
        let count = stream.read(&mut chunk).await.unwrap();
        assert!(count > 0, "connection closed before headers");
        bytes.extend_from_slice(&chunk[..count]);
        assert!(bytes.len() <= 16 * 1024, "headers exceeded limit");
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
    let content_length = headers
        .split("\r\n")
        .skip(1)
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap();
    let mut body = bytes[header_end..].to_vec();
    assert!(body.len() <= content_length);
    body.resize(content_length, 0);
    stream
        .read_exact(&mut body[bytes.len() - header_end..])
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

async fn reply_sse(stream: &mut TcpStream, chunks: &[serde_json::Value]) {
    let mut body = Vec::new();
    for chunk in chunks {
        body.extend_from_slice(
            format!("data: {}\n\n", serde_json::to_string(chunk).unwrap()).as_bytes(),
        );
    }
    body.extend_from_slice(b"data: [DONE]\n\n");
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await.unwrap();
    for chunk in body.chunks(7) {
        stream.write_all(chunk).await.unwrap();
    }
}

#[tokio::test]
async fn sse_accumulates_text_and_tool_call_fragments() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        assert_eq!(request["stream"], true);
        reply_sse(
            &mut stream,
            &[
                json!({"choices":[{"delta":{"content":"hel","reasoning_content":"ignore","tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"bash","arguments":""}}]},"finish_reason":null}]}),
                json!({"choices":[{"delta":{"content":"lo","tool_calls":[{"index":0,"function":{"arguments":r#"{"command":"p"#}}]},"finish_reason":null}]}),
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":r#"wd"}"#}}]},"finish_reason":null}]}),
                json!({"choices":[{"delta":{},"finish_reason":"tool_calls","usage":{"prompt_tokens":174,"completion_tokens":156}}]}),
            ],
        )
        .await;
    });
    let mut model = OpenAiModel::with_timeout(
        format!("http://{address}/v1"),
        "test-key".into(),
        "test-model".into(),
        None,
        false,
        Duration::from_secs(1),
    )
    .unwrap();
    let mut deltas = Vec::new();
    let (message, usage) = model
        .complete(
            &[Message::User {
                content: "hello".into(),
                images: vec![],
            }],
            &[],
            Some(&mut |kind, delta| deltas.push((kind, delta.to_owned()))),
        )
        .await
        .unwrap();
    assert_eq!(
        deltas,
        [
            (ModelDeltaKind::Content, "hel".into()),
            (ModelDeltaKind::Reasoning, "ignore".into()),
            (ModelDeltaKind::Content, "lo".into()),
        ]
    );
    assert_eq!(message.content.as_deref(), Some("hello"));
    assert_eq!(message.reasoning.as_deref(), Some("ignore"));
    assert_eq!(message.tool_calls[0].name, "bash");
    assert_eq!(message.tool_calls[0].arguments, r#"{"command":"pwd"}"#);
    assert_eq!(
        usage,
        Some(Usage {
            input_tokens: 174,
            output_tokens: 156,
        })
    );
}

#[test]
fn wire_messages_never_echo_reasoning() {
    let request = ChatRequest::from_internal(
        "test-model",
        None,
        false,
        &[Message::Assistant(AssistantMessage {
            content: Some("done".into()),
            tool_calls: vec![],
            reasoning: Some("secret thinking".into()),
        })],
        &[],
        None,
    );
    let value = serde_json::to_value(request).unwrap();
    let message = &value["messages"][0];
    assert!(message.get("reasoning").is_none());
    assert!(message.get("reasoning_content").is_none());
}

#[tokio::test]
async fn request_times_out_when_server_stalls() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut streams = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await;
            streams.push(stream);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        drop(streams);
    });
    let mut model = OpenAiModel::with_timeout(
        format!("http://{address}/v1"),
        "test-key".into(),
        "test-model".into(),
        None,
        false,
        Duration::from_millis(50),
    )
    .unwrap();
    let error = model
        .complete(
            &[Message::User {
                content: "hello".into(),
                images: vec![],
            }],
            &[],
            None,
        )
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("timed out"));
}

#[tokio::test]
async fn retries_once_after_a_send_failure() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let _ = read_request(&mut first).await;
        let (mut second, _) = listener.accept().await.unwrap();
        let _ = read_request(&mut second).await;
        reply_sse(
            &mut second,
            &[
                json!({"choices":[{"delta":{"content":"recovered"},"finish_reason":null}]}),
                json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
            ],
        )
        .await;
    });
    let mut model = OpenAiModel::with_timeout(
        format!("http://{address}/v1"),
        "test-key".into(),
        "test-model".into(),
        None,
        false,
        Duration::from_millis(100),
    )
    .unwrap();
    let (message, _) = model
        .complete(
            &[Message::User {
                content: "hello".into(),
                images: vec![],
            }],
            &[],
            None,
        )
        .await
        .unwrap();
    assert_eq!(message.content.as_deref(), Some("recovered"));
}

#[tokio::test]
async fn transient_connect_errors_are_retried_then_fail_with_context() {
    // A port with no listener: every attempt fails with ECONNREFUSED, which
    // reqwest reports as an is_connect() error (same family as the
    // "tls handshake eof" users hit). The chat-wire loop must exhaust all 6
    // attempts (shared with the codex wire) before the error surfaces.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let mut model = OpenAiModel::with_timeout(
        format!("http://{address}/v1"),
        "test-key".into(),
        "test-model".into(),
        None,
        false,
        Duration::from_secs(1),
    )
    .unwrap();
    let started = std::time::Instant::now();
    let error = model
        .complete(
            &[Message::User {
                content: "hello".into(),
                images: vec![],
            }],
            &[],
            None,
        )
        .await
        .unwrap_err();
    // 6 attempts => the sum of all retry backoffs (tests run with a 50ms
    // base, so 50+100+200+400+800ms); this proves all retries ran.
    let expected_sleep: u64 = (1..CONNECT_RETRY_ATTEMPTS)
        .map(|attempt| retry_backoff_ms(CONNECT_RETRY_BASE_BACKOFF_MS, attempt))
        .sum();
    assert!(
        started.elapsed() >= Duration::from_millis(expected_sleep),
        "expected at least {expected_sleep}ms of backoff"
    );
    let text = format!("{error:#}");
    assert!(text.contains("provider request failed"), "{text}");
}

struct FailingTool;

#[async_trait::async_trait]
impl Tool for FailingTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fail".into(),
            description: "fails for test".into(),
            parameters: json!({"type": "object"}),
        }
    }

    async fn execute(&self, _: serde_json::Value) -> Result<String, String> {
        Err("intentional failure".into())
    }
}

#[tokio::test]
async fn real_http_agent_loop_returns_tool_error_to_provider() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let first_request = read_request(&mut first).await;
        reply_sse(
            &mut first,
            &[
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-fail","type":"function","function":{"name":"fail","arguments":"{}"}}]},"finish_reason":null}]}),
                json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
            ],
        )
        .await;
        let (mut second, _) = listener.accept().await.unwrap();
        let second_request = read_request(&mut second).await;
        reply_sse(
            &mut second,
            &[
                json!({"choices":[{"delta":{"content":"final answer"},"finish_reason":null}]}),
                json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
            ],
        )
        .await;
        (first_request, second_request)
    });
    let model = OpenAiModel::with_timeout(
        format!("http://{address}/v1"),
        "test-key".into(),
        "test-model".into(),
        None,
        false,
        Duration::from_secs(1),
    )
    .unwrap();
    let mut agent = Agent::new(Box::new(model), vec![Box::new(FailingTool)]);
    assert_eq!(agent.run("hello".into()).await.unwrap(), "final answer");
    let (first, second) = server.await.unwrap();
    assert_eq!(first["messages"][0]["content"], "hello");
    assert_eq!(second["messages"][1]["role"], "assistant");
    assert_eq!(second["messages"][1]["tool_calls"][0]["id"], "call-fail");
    assert_eq!(second["messages"][2]["role"], "tool");
    assert_eq!(second["messages"][2]["tool_call_id"], "call-fail");
    assert!(
        second["messages"][2]["content"]
            .as_str()
            .unwrap()
            .contains("ERROR: intentional failure")
    );
}

#[test]
fn chat_wire_emits_image_url_object_parts_for_attached_images() {
    let temp = tempfile::tempdir().unwrap();
    let bytes = b"\x89PNG\r\n\x1a\nfake-png-bytes";
    let hash = crate::agent::store_image_bytes(temp.path(), bytes).unwrap();
    let request = ChatRequest::from_internal(
        "vision-model",
        None,
        false,
        &[Message::User {
            content: "what is this?".into(),
            images: vec![ImagePart {
                hash: hash.clone(),
                mime: "image/png".into(),
            }],
        }],
        &[],
        Some(temp.path()),
    );
    let value = serde_json::to_value(request).unwrap();
    // content switches from a plain string to a parts array.
    let parts = value["messages"][0]["content"].as_array().unwrap();
    assert_eq!(parts[0]["type"], "text");
    assert_eq!(parts[0]["text"], "what is this?");
    assert_eq!(parts[1]["type"], "image_url");
    // Chat wire: image_url is an OBJECT with a data-URL `url`.
    let url = parts[1]["image_url"]["url"].as_str().unwrap();
    assert!(url.starts_with("data:image/png;base64,"));
    let encoded = url.trim_start_matches("data:image/png;base64,");
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap(),
        bytes
    );
    // Non-user messages stay plain strings even with images elsewhere.
    let request = ChatRequest::from_internal(
        "vision-model",
        None,
        false,
        &[Message::System {
            content: "sys".into(),
        }],
        &[],
        None,
    );
    let value = serde_json::to_value(request).unwrap();
    assert_eq!(value["messages"][0]["content"], "sys");
}

#[test]
fn chat_wire_degrades_missing_image_file_to_text_placeholder() {
    let request = ChatRequest::from_internal(
        "vision-model",
        None,
        false,
        &[Message::User {
            content: "hi".into(),
            images: vec![ImagePart {
                hash: "deadbeef".into(),
                mime: "image/png".into(),
            }],
        }],
        &[],
        None,
    );
    let value = serde_json::to_value(request).unwrap();
    let parts = value["messages"][0]["content"].as_array().unwrap();
    assert_eq!(parts[1]["type"], "text");
    assert_eq!(parts[1]["text"], "[image missing: deadbeef]");
}

#[test]
fn vision_gate_rejects_images_on_non_vision_models() {
    let error = crate::agent::ensure_vision_supported(
        "deepseek-v3",
        false,
        &[Message::User {
            content: "hi".into(),
            images: vec![ImagePart {
                hash: "x".into(),
                mime: "image/png".into(),
            }],
        }],
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("deepseek-v3 does not support image input"));

    // No images: passes even without vision. Images: passes with vision.
    crate::agent::ensure_vision_supported(
        "deepseek-v3",
        false,
        &[Message::User {
            content: "hi".into(),
            images: vec![],
        }],
    )
    .unwrap();
    crate::agent::ensure_vision_supported(
        "kimi-k3",
        true,
        &[Message::User {
            content: "hi".into(),
            images: vec![ImagePart {
                hash: "x".into(),
                mime: "image/png".into(),
            }],
        }],
    )
    .unwrap();
}

#[tokio::test]
async fn complete_rejects_images_before_any_request_on_non_vision_models() {
    // Nothing listens on this address; the vision gate must fire first.
    let mut model = OpenAiModel::with_timeout(
        "http://127.0.0.1:1".into(),
        "test-key".into(),
        "test-model".into(),
        None,
        false,
        Duration::from_millis(50),
    )
    .unwrap();
    let error = model
        .complete(
            &[Message::User {
                content: "hi".into(),
                images: vec![ImagePart {
                    hash: "x".into(),
                    mime: "image/png".into(),
                }],
            }],
            &[],
            None,
        )
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("does not support image input"));
}

#[tokio::test]
async fn chat_wire_sends_image_url_parts_over_http() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let bytes = b"\x89PNG\r\n\x1a\nfake-http";
    let hash = crate::agent::store_image_bytes(temp.path(), bytes).unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        let parts = request["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(
            parts[1]["image_url"]["url"].as_str().unwrap(),
            format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            )
        );
        reply_sse(
            &mut stream,
            &[
                json!({"choices":[{"delta":{"content":"seen it"},"finish_reason":null}]}),
                json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
            ],
        )
        .await;
    });
    let mut model = OpenAiModel::with_timeout(
        format!("http://{address}/v1"),
        "test-key".into(),
        "test-model".into(),
        None,
        true,
        Duration::from_secs(1),
    )
    .unwrap()
    .with_image_store(temp.path().to_path_buf());
    let (message, _) = model
        .complete(
            &[Message::User {
                content: "what is this?".into(),
                images: vec![ImagePart {
                    hash,
                    mime: "image/png".into(),
                }],
            }],
            &[],
            None,
        )
        .await
        .unwrap();
    assert_eq!(message.content.as_deref(), Some("seen it"));
}
