use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn responses_wire_flattens_system_tools_and_tool_results() {
    let request = ResponsesRequest::from_internal(
        "codex",
        Some("high"),
        &[
            Message::System {
                content: "first".into(),
            },
            Message::System {
                content: "second".into(),
            },
            Message::User {
                content: "hello".into(),
                images: vec![],
            },
            Message::Assistant(AssistantMessage {
                content: Some("working".into()),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                }],
                reasoning: Some("never replayed".into()),
            }),
            Message::Tool {
                call_id: "call-1".into(),
                name: "bash".into(),
                content: "failed".into(),
                is_error: true,
                synthetic: false,
            },
        ],
        &[ToolSpec {
            name: "bash".into(),
            description: "run".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "workspace": {"type": "string"},
                    "task": {"type": "string"},
                }
            }),
        }],
        None,
    );
    let value = serde_json::to_value(request).unwrap();
    assert_eq!(value["instructions"], "first\n\nsecond");
    assert_eq!(value["input"][0]["content"][0]["type"], "input_text");
    assert_eq!(value["input"][2]["type"], "function_call");
    assert_eq!(value["input"][3]["output"], "ERROR: failed");
    assert_eq!(value["tools"][0]["name"], "bash");
    assert!(value["tools"][0].get("function").is_none());
    // parameters.properties must keep input key order (workspace first) on the wire.
    let keys = value["tools"][0]["parameters"]["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(keys, ["workspace", "task"]);
    assert_eq!(value["reasoning"]["effort"], "high");
    assert_eq!(value["include"], json!([]));
}

#[test]
fn responses_wire_emits_input_image_parts_for_attached_images() {
    let temp = tempfile::tempdir().unwrap();
    let bytes = b"\x89PNG\r\n\x1a\nfake-png";
    let hash = crate::agent::store_image_bytes(temp.path(), bytes).unwrap();
    let request = ResponsesRequest::from_internal(
        "codex",
        None,
        &[Message::User {
            content: "what is this?".into(),
            images: vec![crate::agent::ImagePart {
                hash: hash.clone(),
                mime: "image/png".into(),
            }],
        }],
        &[],
        Some(temp.path()),
    );
    let value = serde_json::to_value(request).unwrap();
    let parts = value["input"][0]["content"].as_array().unwrap();
    assert_eq!(parts[0]["type"], "input_text");
    assert_eq!(parts[0]["text"], "what is this?");
    // Codex wire: image_url is a BARE STRING, not an object.
    assert_eq!(parts[1]["type"], "input_image");
    let url = parts[1]["image_url"].as_str().unwrap();
    assert_eq!(
        url,
        format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    );
}

#[test]
fn responses_wire_degrades_missing_image_to_text_placeholder() {
    let request = ResponsesRequest::from_internal(
        "codex",
        None,
        &[Message::User {
            content: "hi".into(),
            images: vec![crate::agent::ImagePart {
                hash: "missing-hash".into(),
                mime: "image/png".into(),
            }],
        }],
        &[],
        None,
    );
    let value = serde_json::to_value(request).unwrap();
    let parts = value["input"][0]["content"].as_array().unwrap();
    assert_eq!(parts[1]["type"], "input_text");
    assert_eq!(parts[1]["text"], "[image missing: missing-hash]");
}

#[test]
fn usage_requires_completed_usage_fields() {
    assert_eq!(
        response_usage(&json!({"input_tokens": 3, "output_tokens": 4})).unwrap(),
        Usage {
            input_tokens: 3,
            output_tokens: 4
        }
    );
    assert!(response_usage(&json!({"input_tokens": 3})).is_err());
}

#[tokio::test]
async fn local_sse_emits_text_reasoning_function_call_and_usage() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/responses", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let count = stream.read(&mut request).await.unwrap();
        let request = String::from_utf8_lossy(&request[..count]).to_ascii_lowercase();
        assert!(request.contains("chatgpt-account-id: account"));
        assert!(request.contains("user-agent: codex_cli_rs/"));
        assert!(request.contains("originator: codex_cli_rs"));
        let body = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
            "data: {\"type\":\"response.reasoning_summary_part.added\",\"summary_index\":0}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"**Planning**\"}\n\n",
            "data: {\"type\":\"response.reasoning_summary_part.added\",\"summary_index\":1}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"**Working**\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"bash\",\"arguments\":\"{}\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":4}}}\n\n"
        );
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
    });
    let temp = tempfile::tempdir().unwrap();
    let mut model = CodexModel::with_endpoint(
        crate::codex_auth::CodexAuth::test_auth(temp.path().join("auth.json")),
        endpoint,
    );
    let mut deltas = Vec::new();
    let (message, usage) = model
        .complete(
            &[Message::User {
                content: "hello".into(),
                images: vec![],
            }],
            &[],
            Some(&mut |kind, text| deltas.push((kind, text.to_owned()))),
        )
        .await
        .unwrap();
    assert_eq!(message.content.as_deref(), Some("hi"));
    assert_eq!(
        message.reasoning.as_deref(),
        Some("**Planning**\n\n**Working**")
    );
    assert_eq!(message.tool_calls[0].id, "c1");
    assert_eq!(
        usage,
        Some(Usage {
            input_tokens: 3,
            output_tokens: 4
        })
    );
    assert_eq!(
        deltas,
        [
            (ModelDeltaKind::Content, "hi".into()),
            (ModelDeltaKind::Reasoning, "**Planning**".into()),
            (ModelDeltaKind::Reasoning, "\n\n".into()),
            (ModelDeltaKind::Reasoning, "**Working**".into()),
        ]
    );
}

async fn sse_failure(body: String) -> anyhow::Error {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/responses", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let _ = stream.read(&mut request).await.unwrap();
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
    });
    let temp = tempfile::tempdir().unwrap();
    let mut model = CodexModel::with_endpoint(
        crate::codex_auth::CodexAuth::test_auth(temp.path().join("auth.json")),
        endpoint,
    );
    model
        .complete(
            &[Message::User {
                content: "hello".into(),
                images: vec![],
            }],
            &[],
            None,
        )
        .await
        .unwrap_err()
}

#[tokio::test]
async fn sse_reports_failed_incomplete_and_explicit_error_events() {
    let failed = sse_failure(
        "data: {\"type\":\"response.failed\",\"error\":{\"message\":\"failed reason\"}}\n\n".into(),
    )
    .await;
    assert!(format!("{failed:#}").contains("failed reason"));
    let incomplete = sse_failure("data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n".into()).await;
    assert!(format!("{incomplete:#}").contains("max_output_tokens"));
    let explicit =
        sse_failure("data: {\"type\":\"error\",\"message\":\"explicit reason\"}\n\n".into()).await;
    assert!(format!("{explicit:#}").contains("explicit reason"));
    let response_error = sse_failure(
        "data: {\"type\":\"response.error\",\"error\":{\"message\":\"response reason\"}}\n\n"
            .into(),
    )
    .await;
    assert!(format!("{response_error:#}").contains("response reason"));
}

#[tokio::test]
async fn sse_requires_completed_before_done_or_eof() {
    for body in [
        "data: [DONE]\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
    ] {
        let error = sse_failure(body.into()).await;
        assert!(format!("{error:#}").contains("before response.completed"));
    }
}

#[tokio::test]
async fn backend_401_refreshes_once_persists_rotation_and_retries_once() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/responses", listener.local_addr().unwrap());
    let token_endpoint = format!("http://{}/token", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let count = first.read(&mut request).await.unwrap();
        assert!(
            String::from_utf8_lossy(&request[..count])
                .to_ascii_lowercase()
                .contains("authorization: bearer access")
        );
        first
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let (mut token, _) = listener.accept().await.unwrap();
        let count = token.read(&mut request).await.unwrap();
        assert!(String::from_utf8_lossy(&request[..count]).contains("refresh"));
        let refresh = r#"{"access_token":"rotated-access","refresh_token":"rotated-refresh"}"#;
        token.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", refresh.len(), refresh).as_bytes()).await.unwrap();
        let (mut second, _) = listener.accept().await.unwrap();
        let count = second.read(&mut request).await.unwrap();
        assert!(
            String::from_utf8_lossy(&request[..count])
                .to_ascii_lowercase()
                .contains("authorization: bearer rotated-access")
        );
        let done = "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n\n";
        second.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", done.len(), done).as_bytes()).await.unwrap();
    });
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("auth.json");
    let auth =
        crate::codex_auth::CodexAuth::test_auth(path.clone()).with_token_endpoint(token_endpoint);
    let mut model = CodexModel::with_endpoint(auth.clone(), endpoint);
    let (_, usage) = model
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
    server.await.unwrap();
    assert_eq!(
        usage,
        Some(Usage {
            input_tokens: 1,
            output_tokens: 2
        })
    );
    assert_eq!(
        auth.current_access_token_and_account().await.unwrap().0,
        "rotated-access"
    );
    assert!(
        std::fs::read_to_string(path)
            .unwrap()
            .contains("rotated-refresh")
    );
}

#[test]
fn retry_backoff_doubles_exponentially_within_a_63s_window() {
    // Production schedule (base 500ms, 8 attempts): the sleeps before attempts
    // 2..=8 are 0.5/1/2/4/8/16/32s — a ~63.5s recovery window for jittery
    // peak-hour networks (codex flakiness).
    let sleeps: Vec<u64> = (1..CONNECT_RETRY_ATTEMPTS)
        .map(|attempt| retry_backoff_ms(500, attempt))
        .collect();
    assert_eq!(sleeps, [500, 1000, 2000, 4000, 8000, 16000, 32000]);
    let total: u64 = sleeps.iter().sum();
    assert!(
        total >= 60_000,
        "backoff window must be >= 60s, got {total}ms"
    );
    // Doubling invariant holds for any base.
    for attempt in 2..CONNECT_RETRY_ATTEMPTS {
        assert_eq!(
            retry_backoff_ms(500, attempt),
            2 * retry_backoff_ms(500, attempt - 1)
        );
    }
}

#[tokio::test]
async fn transient_connect_errors_are_retried_then_fail_with_context() {
    // A port with no listener: every attempt fails with ECONNREFUSED, which
    // reqwest reports as an is_connect() error (same family as the
    // "tls handshake eof" users hit). The loop must exhaust all 8 attempts
    // before the context-wrapped error surfaces.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let temp = tempfile::tempdir().unwrap();
    let mut model = CodexModel::with_endpoint(
        crate::codex_auth::CodexAuth::test_auth(temp.path().join("auth.json")),
        format!("http://{addr}/responses"),
    );
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
    // 8 attempts => the sum of all retry backoffs (tests run with a 10ms
    // base, so 10+20+40+80+160+320+640ms); this proves all retries ran.
    let expected_sleep: u64 = (1..CONNECT_RETRY_ATTEMPTS)
        .map(|attempt| retry_backoff_ms(CONNECT_RETRY_BASE_BACKOFF_MS, attempt))
        .sum();
    assert!(
        started.elapsed() >= Duration::from_millis(expected_sleep),
        "expected at least {expected_sleep}ms of backoff"
    );
    let text = format!("{error:#}");
    assert!(text.contains("ChatGPT Responses request failed"), "{text}");
}

#[tokio::test]
async fn transient_connect_error_retries_and_recovers() {
    // Bind a listener to learn a free port, then drop it: the model's first
    // send attempt hits ECONNREFUSED (is_connect()). The mock server rebinds
    // that port shortly after, so the retry lands on a live server.
    let first = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = first.local_addr().unwrap();
    drop(first);
    let endpoint = format!("http://{addr}/responses");
    let server = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let count = stream.read(&mut request).await.unwrap();
        assert!(
            String::from_utf8_lossy(&request[..count])
                .to_ascii_lowercase()
                .contains("authorization: bearer access")
        );
        let done = "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n\n";
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    done.len(),
                    done
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });
    let temp = tempfile::tempdir().unwrap();
    let mut model = CodexModel::with_endpoint(
        crate::codex_auth::CodexAuth::test_auth(temp.path().join("auth.json")),
        endpoint,
    );
    let (message, usage) = model
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
    server.await.unwrap();
    assert_eq!(message.content, None);
    assert_eq!(
        usage,
        Some(Usage {
            input_tokens: 1,
            output_tokens: 2
        })
    );
}

#[tokio::test]
async fn http_5xx_is_retried_then_succeeds() {
    // Two 503s (server overloaded), then a completed stream: the codex wire
    // retries transient HTTP errors (same whole-request loop as the chat
    // wire) and recovers on the third attempt.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/responses", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        }
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let _ = stream.read(&mut request).await.unwrap();
        let done = "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n\n";
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    done.len(),
                    done
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });
    let temp = tempfile::tempdir().unwrap();
    let mut model = CodexModel::with_endpoint(
        crate::codex_auth::CodexAuth::test_auth(temp.path().join("auth.json")),
        endpoint,
    );
    let (message, usage) = model
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
    server.await.unwrap();
    assert_eq!(message.content, None);
    assert_eq!(
        usage,
        Some(Usage {
            input_tokens: 1,
            output_tokens: 2
        })
    );
}

#[tokio::test]
async fn http_5xx_is_retried_then_fails_with_status() {
    // 503 on every attempt: after HTTP_RETRY_ATTEMPTS the error surfaces with
    // the status code, having run the full backoff schedule.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/responses", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        for _ in 0..HTTP_RETRY_ATTEMPTS {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        }
    });
    let temp = tempfile::tempdir().unwrap();
    let mut model = CodexModel::with_endpoint(
        crate::codex_auth::CodexAuth::test_auth(temp.path().join("auth.json")),
        endpoint,
    );
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
    // 3 attempts => the sum of the retry backoffs (tests run with a 10ms
    // base, so 10+20ms) before the final 503 surfaces.
    let expected_sleep: u64 = (1..HTTP_RETRY_ATTEMPTS)
        .map(|attempt| retry_backoff_ms(HTTP_RETRY_BASE_BACKOFF_MS, attempt))
        .sum();
    assert!(
        started.elapsed() >= Duration::from_millis(expected_sleep),
        "expected at least {expected_sleep}ms of backoff"
    );
    let text = format!("{error:#}");
    assert!(text.contains("HTTP 503"), "{text}");
    server.await.unwrap();
}

#[tokio::test]
async fn stream_decode_error_is_retried_then_succeeds() {
    // First response delivers a data frame that is not valid JSON: the stream
    // dies mid-flight with "cannot decode ChatGPT Responses event", and the
    // whole idempotent request is retried, landing on a healthy stream.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/responses", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let _ = stream.read(&mut request).await.unwrap();
        let bad = b"data: {not-json}\n\n";
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bad.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        stream.write_all(bad).await.unwrap();
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let _ = stream.read(&mut request).await.unwrap();
        let done = "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n\n";
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    done.len(),
                    done
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });
    let temp = tempfile::tempdir().unwrap();
    let mut model = CodexModel::with_endpoint(
        crate::codex_auth::CodexAuth::test_auth(temp.path().join("auth.json")),
        endpoint,
    );
    let (message, usage) = model
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
    server.await.unwrap();
    assert_eq!(message.content, None);
    assert_eq!(
        usage,
        Some(Usage {
            input_tokens: 1,
            output_tokens: 2
        })
    );
}
