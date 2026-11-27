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
                images: vec![],
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
        crate::images::DEFAULT_MAX_INLINE,
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
    assert_eq!(value["reasoning"]["summary"], "auto");
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
        crate::images::DEFAULT_MAX_INLINE,
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
        crate::images::DEFAULT_MAX_INLINE,
    );
    let value = serde_json::to_value(request).unwrap();
    let parts = value["input"][0]["content"].as_array().unwrap();
    assert_eq!(parts[1]["type"], "input_text");
    assert_eq!(parts[1]["text"], "[image missing: missing-hash]");
}

#[test]
fn responses_wire_encodes_image_bearing_tool_output_as_output_array() {
    // An image-bearing Tool result is encoded natively: function_call_output
    // with `output` as an ARRAY of input_text + input_image parts (scalar
    // string for plain text results, see
    // responses_wire_flattens_system_tools_and_tool_results).
    let temp = tempfile::tempdir().unwrap();
    let bytes = b"\x89PNG\r\n\x1a\nfake";
    let hash = crate::agent::store_image_bytes(temp.path(), bytes).unwrap();
    let request = ResponsesRequest::from_internal(
        "codex",
        None,
        &[
            Message::Assistant(AssistantMessage {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "read_image".into(),
                    arguments: r#"{"path":"a.png"}"#.into(),
                }],
                reasoning: None,
            }),
            Message::Tool {
                call_id: "call-1".into(),
                name: "read_image".into(),
                content: "[image read: a.png] (hash x, image/png, 4 bytes)".into(),
                images: vec![crate::agent::ImagePart {
                    hash,
                    mime: "image/png".into(),
                }],
                is_error: false,
                synthetic: false,
            },
        ],
        &[],
        Some(temp.path()),
        crate::images::DEFAULT_MAX_INLINE,
    );
    let value = serde_json::to_value(request).unwrap();
    // content: None => the assistant contributes only the function_call
    // item, so the function_call_output lands at input[1].
    let output = &value["input"][1]["output"];
    assert!(output.is_array(), "image-bearing output must be an array");
    assert_eq!(output[0]["type"], "input_text");
    assert_eq!(
        output[0]["text"],
        "[image read: a.png] (hash x, image/png, 4 bytes)"
    );
    assert_eq!(output[1]["type"], "input_image");
    // Codex wire: image_url is a BARE STRING, not an object.
    assert_eq!(
        output[1]["image_url"].as_str().unwrap(),
        format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    );
}

#[test]
fn responses_wire_degrades_missing_tool_image_to_text_part() {
    // A missing store file on an image-bearing tool degrades to an
    // input_text placeholder part inside the output array.
    let request = ResponsesRequest::from_internal(
        "codex",
        None,
        &[
            Message::Assistant(AssistantMessage {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "read_image".into(),
                    arguments: r#"{"path":"a.png"}"#.into(),
                }],
                reasoning: None,
            }),
            Message::Tool {
                call_id: "call-1".into(),
                name: "read_image".into(),
                content: "[image read: a.png]".into(),
                images: vec![crate::agent::ImagePart {
                    hash: "gone-hash".into(),
                    mime: "image/png".into(),
                }],
                is_error: false,
                synthetic: false,
            },
        ],
        &[],
        None,
        crate::images::DEFAULT_MAX_INLINE,
    );
    let value = serde_json::to_value(request).unwrap();
    let output = &value["input"][1]["output"];
    assert!(output.is_array());
    assert_eq!(output[0]["type"], "input_text");
    assert_eq!(output[1]["type"], "input_text");
    assert_eq!(output[1]["text"], "[image missing: gone-hash]");
}

#[test]
fn usage_requires_completed_usage_fields() {
    // The response payload carries the nested `usage` object; optional
    // enrichment fields are parsed when present.
    assert_eq!(
        response_usage(&json!({"usage": {"input_tokens": 3, "output_tokens": 4}}))
            .unwrap()
            .unwrap(),
        Usage {
            input_tokens: 3,
            output_tokens: 4,
            ..Default::default()
        }
    );
    // Incomplete usage (missing output_tokens) is an error…
    assert!(response_usage(&json!({"usage": {"input_tokens": 3}})).is_err());
    // …and a response with NO usage object yields Ok(None) (not an error).
    assert_eq!(response_usage(&json!({"input_tokens": 3})).unwrap(), None);
    // Enrichment fields are threaded through when present.
    assert_eq!(
        response_usage(&json!({"finish_reason": "stop", "usage": {
            "input_tokens": 100,
            "output_tokens": 50,
            "input_tokens_details": {"cached_tokens": 30},
            "output_tokens_details": {"reasoning_tokens": 20}
        }}))
        .unwrap()
        .unwrap(),
        Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_hit_tokens: Some(30),
            cache_miss_tokens: Some(70),
            reasoning_tokens: Some(20),
            finish_reason: Some("stop".into()),
        }
    );
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
            output_tokens: 4,
            ..Default::default()
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
            output_tokens: 2,
            ..Default::default()
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
            output_tokens: 2,
            ..Default::default()
        })
    );
}

#[tokio::test]
async fn image_rejection_is_typed_and_bypasses_the_retryable_403_path() {
    // The codex wire shares the typed rejection: a 403 naming images with
    // image references submitted is classified after exactly one request
    // (403 is otherwise retried), carrying the full diagnostic.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/responses", listener.local_addr().unwrap());
    let body = r#"{"error":{"message":"image input is not supported"}}"#;
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let server_attempts = attempts.clone();
    tokio::spawn(async move {
        for _ in 0..HTTP_RETRY_ATTEMPTS {
            let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(std::time::Duration::from_millis(150), listener.accept())
                    .await
            else {
                break;
            };
            server_attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut request = [0; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    });
    let temp = tempfile::tempdir().unwrap();
    let store = temp.path().join("images");
    std::fs::create_dir_all(&store).unwrap();
    // The image must REALLY be on the wire for the rejection to claim it.
    std::fs::write(store.join("hash-a"), tiny_png()).unwrap();
    let mut model = CodexModel::with_endpoint(
        crate::codex_auth::CodexAuth::test_auth(temp.path().join("auth.json")),
        endpoint,
    )
    .with_vision(true);
    model.image_store = Some(store);
    let error = model
        .complete(
            &[Message::User {
                content: "what is this?".into(),
                images: vec![crate::agent::ImagePart {
                    hash: "hash-a".into(),
                    mime: "image/png".into(),
                }],
            }],
            &[],
            None,
        )
        .await
        .unwrap_err();
    let rejection = crate::model::image_rejection(&error).expect("typed image rejection");
    assert_eq!(rejection.status, reqwest::StatusCode::FORBIDDEN);
    assert_eq!(rejection.diagnostic, body);
    assert_eq!(rejection.hashes, vec!["hash-a".to_string()]);
    assert!(format!("{error:#}").contains(body));
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
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
            output_tokens: 2,
            ..Default::default()
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
            output_tokens: 2,
            ..Default::default()
        })
    );
}

#[tokio::test]
async fn responses_request_hot_reloads_disk_login_headers() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/responses", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        for (access, account) in [("access", "account"), ("new-access", "new-account")] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let count = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..count]).to_ascii_lowercase();
            assert!(request.contains(&format!("authorization: bearer {access}")));
            assert!(request.contains(&format!("chatgpt-account-id: {account}")));
            let body = "data: {\"type\":\"response.completed\",\"response\":{}}\n\n";
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
        }
    });
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("auth.json");
    let auth = crate::codex_auth::CodexAuth::test_auth(path.clone());
    let mut model = CodexModel::with_endpoint(auth, endpoint);
    let messages = [Message::User {
        content: "hello".into(),
        images: vec![],
    }];
    model.complete(&messages, &[], None).await.unwrap();
    std::fs::write(
        &path,
        format!(
            r#"{{"tokens":{{"id_token":"new-id","access_token":"new-access","refresh_token":"new-refresh","account_id":"new-account"}},"last_refresh":"{}"}}"#,
            chrono::Utc::now().to_rfc3339(),
        ),
    )
    .unwrap();
    model.complete(&messages, &[], None).await.unwrap();
    server.await.unwrap();
}

/// A tiny valid PNG: below the token budget, so it is sent byte-identical.
fn tiny_png() -> Vec<u8> {
    let image =
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(8, 8, image::Rgb([1, 2, 3])));
    let mut bytes = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    bytes
}

/// A 1920x1080 PNG over the token budget: normalises to 1708x966.
fn wide_png(rgb: [u8; 3]) -> Vec<u8> {
    let image =
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(1920, 1080, image::Rgb(rgb)));
    let mut bytes = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    bytes
}

/// The Responses wire applies the newest-image rule too: the image appended
/// after the newest assistant message goes out byte-identical with its MIME,
/// while the delivered one before it keeps the ordinary resize.
#[test]
fn responses_wire_sends_the_newest_image_full_size_and_resizes_the_old_one() {
    let store = tempfile::tempdir().unwrap();
    let old = wide_png([200, 30, 30]);
    let new = wide_png([30, 30, 200]);
    let (old_hash, new_hash) = (
        crate::agent::image_sha256(&old),
        crate::agent::image_sha256(&new),
    );
    std::fs::write(store.path().join(&old_hash), &old).unwrap();
    std::fs::write(store.path().join(&new_hash), &new).unwrap();
    let messages = vec![
        Message::User {
            content: "old".into(),
            images: vec![ImagePart {
                hash: old_hash,
                mime: "image/png".into(),
            }],
        },
        Message::Assistant(AssistantMessage {
            content: Some("seen".into()),
            tool_calls: Vec::new(),
            reasoning: None,
        }),
        Message::User {
            content: "new".into(),
            images: vec![ImagePart {
                hash: new_hash,
                mime: "image/png".into(),
            }],
        },
    ];
    let request = ResponsesRequest::from_internal(
        "codex",
        None,
        &messages,
        &[],
        Some(store.path()),
        crate::images::DEFAULT_MAX_INLINE,
    );
    let value = serde_json::to_value(&request).unwrap();
    let input = value["input"].as_array().unwrap();
    let data = |part: &serde_json::Value| {
        let url = part["image_url"].as_str().unwrap();
        let (mime, payload) = url
            .strip_prefix("data:")
            .unwrap()
            .split_once(";base64,")
            .unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .unwrap();
        (mime.to_owned(), bytes)
    };

    let (mime, resized) = data(&input[0]["content"].as_array().unwrap()[1]);
    assert_eq!(mime, "image/jpeg", "the older image keeps the budget");
    let decoded = image::load_from_memory(&resized).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (1708, 966));

    let (mime, full) = data(&input[2]["content"].as_array().unwrap()[1]);
    assert_eq!(mime, "image/png", "the newest image keeps its MIME");
    assert_eq!(full, new, "the newest image is byte-identical to the store");
}

/// Ten distinct user images in one request: the serialized Responses body
/// keeps the newest six as `input_image` parts and replaces the oldest four
/// with recovery placeholders, in place.
#[test]
fn responses_wire_offloads_the_oldest_images() {
    let store = tempfile::tempdir().unwrap();
    let png = tiny_png();
    let mut messages: Vec<Message> = (0..10)
        .map(|index| {
            let hash = format!("img{index}");
            std::fs::write(store.path().join(&hash), &png).unwrap();
            Message::User {
                content: format!("image {index}"),
                images: vec![ImagePart {
                    hash,
                    mime: "image/png".into(),
                }],
            }
        })
        .collect();
    // A completed model response: all ten images are delivered.
    messages.push(Message::Assistant(AssistantMessage {
        content: Some("seen".into()),
        tool_calls: Vec::new(),
        reasoning: None,
    }));
    let body = |max_inline: usize| {
        serde_json::to_string(&ResponsesRequest::from_internal(
            "codex",
            None,
            &messages,
            &[],
            Some(store.path()),
            max_inline,
        ))
        .unwrap()
    };
    let value: serde_json::Value = serde_json::from_str(&body(6)).unwrap();
    let input = value["input"].as_array().unwrap();
    assert_eq!(input.len(), 11, "ten images plus the assistant response");
    for (index, item) in input[..10].iter().enumerate() {
        let parts = item["content"].as_array().unwrap();
        let images: Vec<_> = parts
            .iter()
            .filter(|part| part["type"] == "input_image")
            .collect();
        let placeholders: Vec<_> = parts
            .iter()
            .filter(|part| {
                part["type"] == "input_text"
                    && part["text"]
                        .as_str()
                        .is_some_and(|text| text.contains("image omitted"))
            })
            .collect();
        if index < 4 {
            assert!(images.is_empty(), "old image {index} must be offloaded");
            assert_eq!(
                placeholders[0]["text"],
                format!(
                    "[image omitted: 4 older image(s); read again with read_image (hash img{index})]"
                )
            );
        } else {
            assert_eq!(images.len(), 1, "the newest six stay inline");
            assert!(placeholders.is_empty());
            assert_eq!(
                images[0]["image_url"],
                format!(
                    "data:image/png;base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(&png)
                ),
                "a fitting image is sent byte-identical"
            );
        }
    }
    // Pure and deterministic: the same messages serialise to the same bytes.
    assert_eq!(body(6), body(6));
    // `max_inline = 0` disables offloading: every image stays inline.
    let all: serde_json::Value = serde_json::from_str(&body(0)).unwrap();
    let inline = all["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| {
            item["content"]
                .as_array()
                .is_some_and(|parts| parts.iter().any(|part| part["type"] == "input_image"))
        })
        .count();
    assert_eq!(inline, 10);
    // The transmitted record names only the six images really on the wire.
    let request =
        ResponsesRequest::from_internal("codex", None, &messages, &[], Some(store.path()), 6);
    assert_eq!(
        request.transmitted_images,
        (4..10)
            .map(|index| format!("img{index}"))
            .collect::<Vec<_>>()
    );
}

/// Finding 1 on the Responses wire: images appended after the newest
/// assistant message have never been transmitted, so a batch larger than
/// `max_inline` still goes out whole; only after a response may old images be
/// deferred.
#[test]
fn responses_wire_keeps_a_new_image_batch_inline_until_delivered() {
    let store = tempfile::tempdir().unwrap();
    let png = tiny_png();
    let mut messages: Vec<Message> = (0..8)
        .map(|index| {
            let hash = format!("new{index}");
            std::fs::write(store.path().join(&hash), &png).unwrap();
            Message::User {
                content: format!("image {index}"),
                images: vec![ImagePart {
                    hash,
                    mime: "image/png".into(),
                }],
            }
        })
        .collect();
    let request =
        ResponsesRequest::from_internal("codex", None, &messages, &[], Some(store.path()), 2);
    assert_eq!(
        request.transmitted_images.len(),
        8,
        "every brand-new image gets its first send"
    );
    let value = serde_json::to_value(&request).unwrap();
    for item in value["input"].as_array().unwrap() {
        assert!(
            !item["content"]
                .as_array()
                .unwrap()
                .iter()
                .any(|part| part["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("image omitted"))),
            "nothing is offloaded before it is delivered"
        );
    }

    messages.push(Message::Assistant(AssistantMessage {
        content: Some("seen".into()),
        tool_calls: Vec::new(),
        reasoning: None,
    }));
    let request =
        ResponsesRequest::from_internal("codex", None, &messages, &[], Some(store.path()), 2);
    assert_eq!(
        request.transmitted_images,
        vec!["new6".to_owned(), "new7".to_owned()],
        "after delivery only the newest two stay inline"
    );
}
