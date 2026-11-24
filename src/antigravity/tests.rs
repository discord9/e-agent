use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{Duration as ChronoDuration, Utc};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::*;
use crate::agent::{Message, Model, ToolSpec};

/// One recorded request: path, raw headers (lowercased compare on demand) and
/// the decoded JSON body.
type Recorded = Arc<Mutex<Vec<(String, String, Value)>>>;

/// Test-only stand-ins for the `agy` install's OAuth consumer client (the
/// real one lives in the config, never in the binary).
const TEST_CLIENT_ID: &str = "test-client.apps.googleusercontent.com";
const TEST_CLIENT_SECRET: &str = "GOCSPX-test";

struct Fake {
    address: std::net::SocketAddr,
    requests: Recorded,
    /// Undecoded request bodies, index-aligned with `requests` (a form-encoded
    /// token refresh is not JSON, so it would otherwise be lost).
    raw: Arc<Mutex<Vec<String>>>,
    /// Arrival time of each request, index-aligned with `requests`: the
    /// pacing tests measure these, not the client's own clock.
    instants: Arc<Mutex<Vec<Instant>>>,
}

impl Fake {
    fn requests(&self) -> Vec<(String, String, Value)> {
        self.requests.lock().unwrap().clone()
    }

    fn raw_bodies(&self) -> Vec<String> {
        self.raw.lock().unwrap().clone()
    }

    /// Gaps between consecutive request arrivals.
    fn gaps(&self) -> Vec<std::time::Duration> {
        let instants = self.instants.lock().unwrap().clone();
        instants.windows(2).map(|pair| pair[1] - pair[0]).collect()
    }
}

/// The interval the pacing tests configure: far below the 1000 ms default, so
/// the tests stay fast, and far above any unpaced gap (~0.2 ms).
const PACING_TEST_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Assert one measured arrival gap is paced. An arrival stamp carries the
/// client's per-request DISPATCH latency (wake-up after the sleep, request
/// build, pooled socket write), which the pacer neither observes nor controls:
/// under a loaded test runtime that jitter reaches tens of milliseconds, so the
/// floor is 60% of the interval. It still sits ~300x above an UNPACED gap —
/// what this discriminates against — and the exact wait is checked separately
/// on the client's own clock, where no jitter exists.
fn assert_paced(gap: std::time::Duration, interval: std::time::Duration, seen: &str) {
    let floor = interval * 3 / 5;
    assert!(
        gap >= floor,
        "gap {gap:?} < {floor:?} of {interval:?}: {seen}"
    );
}

/// Serve exactly `expected` one-shot HTTP connections on a local port,
/// answering the `index`-th request with `handler(index, path)`.
async fn spawn_fake<F>(expected: usize, handler: F) -> Fake
where
    F: Fn(usize, &str) -> (u16, String) + Send + 'static,
{
    spawn_responder(expected, move |index, path| {
        let (status, payload) = handler(index, path);
        let reason = if status == 200 { "OK" } else { "Error" };
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        )
    })
    .await
}

/// [`spawn_fake`] with the whole response text under the caller's control (a
/// redirect status and its `Location` header, for one).
async fn spawn_responder<F>(expected: usize, respond: F) -> Fake
where
    F: Fn(usize, &str) -> String + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests: Recorded = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let raw: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let raw_bodies = raw.clone();
    let instants: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
    let arrivals = instants.clone();
    tokio::spawn(async move {
        for index in 0..expected {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            arrivals.lock().unwrap().push(Instant::now());
            let (headers, body) = read_request(&mut stream).await;
            let path = headers
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or_default()
                .to_owned();
            recorded
                .lock()
                .unwrap()
                .push((path.clone(), headers, body.0.clone()));
            raw_bodies.lock().unwrap().push(body.1.clone());
            let response = respond(index, &path);
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    Fake {
        address,
        requests,
        raw,
        instants,
    }
}

/// The decoded JSON body plus the raw bytes of one request.
async fn read_request(stream: &mut TcpStream) -> (String, (Value, String)) {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0; 2048];
        let count = stream.read(&mut chunk).await.unwrap();
        assert!(count > 0, "connection closed before headers");
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..header_end])
        .unwrap()
        .to_owned();
    let content_length = headers
        .split("\r\n")
        .skip(1)
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    let mut body = bytes[header_end..].to_vec();
    while body.len() < content_length {
        let mut chunk = vec![0; content_length - body.len()];
        let count = stream.read(&mut chunk).await.unwrap();
        assert!(count > 0, "connection closed mid-body");
        body.extend_from_slice(&chunk[..count]);
    }
    let raw = String::from_utf8_lossy(&body).into_owned();
    (
        headers,
        (serde_json::from_slice(&body).unwrap_or(Value::Null), raw),
    )
}

/// Wrap elements the way the service frames them: `[\n{..}\n,\n{..}\n]`.
fn array_stream(elements: &[Value]) -> String {
    let body = elements
        .iter()
        .map(|element| serde_json::to_string_pretty(element).unwrap())
        .collect::<Vec<_>>()
        .join("\n,\n");
    format!("[\n{body}\n]")
}

fn write_agy_token(dir: &Path, access: &str, expiry: DateTime<Utc>, refresh: &str) -> PathBuf {
    let path = dir.join("antigravity-oauth-token");
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({
            "token": {
                "access_token": access,
                "refresh_token": refresh,
                "expiry": expiry.to_rfc3339(),
                "token_type": "Bearer",
            }
        }))
        .unwrap(),
    )
    .unwrap();
    path
}

/// e-agent's own cache file, as a previous login left it.
fn write_cache(path: &Path, access: &str, expiry: DateTime<Utc>, refresh: &str, cli: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        serde_json::to_vec(&json!({
            "token": {
                "access_token": access,
                "refresh_token": refresh,
                "expiry": expiry.to_rfc3339(),
            },
            "cli_fingerprint": login_fingerprint(cli),
        }))
        .unwrap(),
    )
    .unwrap();
}

fn model_for(fake: &Fake, token_file: PathBuf, cache_file: PathBuf) -> AntigravityModel {
    AntigravityModel {
        // The production client shape (redirect policy included), only with
        // the local test endpoint reachable directly and a short timeout.
        client: AntigravityModel::client_builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap(),
        model: "gemini-3.8-flash-tiered".into(),
        reasoning_effort: None,
        stream_url: format!("http://{}/v1internal:streamGenerateContent", fake.address),
        token_endpoint: format!("http://{}/token", fake.address),
        token_file,
        cache_file,
        client_id: Some(TEST_CLIENT_ID.into()),
        client_secret: Some(TEST_CLIENT_SECRET.into()),
        // Unpaced by default so the other tests keep their timing; the pacing
        // tests set a real interval on the model.
        min_interval: std::time::Duration::ZERO,
        last_request: Default::default(),
    }
}

fn text_element(text: &str) -> Value {
    json!({
        "response": {
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": text}]},
                "finishReason": "STOP",
            }],
            "usageMetadata": {
                "promptTokenCount": 3,
                "candidatesTokenCount": 2,
                "thoughtsTokenCount": 7,
                "totalTokenCount": 12,
            },
        },
        "traceId": "trace",
        "metadata": {},
    })
}

#[tokio::test]
async fn streams_text_deltas_and_maps_usage() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "test-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-token",
    );
    let body = array_stream(&[
        json!({"response": {"candidates": [{"content": {"role": "model", "parts": [{"text": "O"}]}}]}}),
        // A later element settles the counts; the trailing signature part and
        // the empty text it carries are not content.
        json!({"response": {
            "candidates": [{"content": {"role": "model", "parts": [
                {"text": "K"},
                {"thoughtSignature": "sig-text", "text": ""},
            ]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 2, "thoughtsTokenCount": 7},
        }}),
    ]);
    let fake = spawn_fake(1, move |_, _| (200, body.clone())).await;
    let mut model = model_for(
        &fake,
        token_file.clone(),
        dir.path().join("cache/antigravity-token.json"),
    );
    let mut deltas: Vec<(ModelDeltaKind, String)> = Vec::new();
    let mut sink = |kind: ModelDeltaKind, text: &str| deltas.push((kind, text.to_owned()));
    let spec = ToolSpec {
        name: "bash".into(),
        description: "run a command".into(),
        parameters: json!({"type": "object"}),
    };
    let (assistant, usage) = model
        .complete(
            &[
                Message::System {
                    content: "be brief".into(),
                },
                Message::User {
                    content: "say ok".into(),
                    images: Vec::new(),
                },
            ],
            &[spec],
            Some(&mut sink),
        )
        .await
        .unwrap();
    // Every delta is content: thought text never reaches the caller as a
    // reasoning delta on this wire.
    assert_eq!(
        deltas,
        [
            (ModelDeltaKind::Content, "O".to_owned()),
            (ModelDeltaKind::Content, "K".to_owned()),
        ]
    );
    assert_eq!(assistant.content.as_deref(), Some("OK"));
    assert!(assistant.tool_calls.is_empty());
    let usage = usage.expect("usage reported");
    assert_eq!(usage.input_tokens, 3);
    assert_eq!(usage.output_tokens, 9, "candidates + thoughts");
    assert_eq!(usage.reasoning_tokens, Some(7));
    assert_eq!(usage.finish_reason.as_deref(), Some("STOP"));

    // The CLI's own token file is used as-is (no refresh) and the request
    // carries the verified headers: bearer token, exact user agent, and no
    // `x-goog-api-client` (it triggers the "version no longer supported").
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    let (path, headers, body) = &requests[0];
    assert_eq!(path, "/v1internal:streamGenerateContent");
    let lower = headers.to_ascii_lowercase();
    assert!(
        lower.contains("authorization: bearer test-token"),
        "{lower}"
    );
    assert!(
        headers.contains(&format!("user-agent: {USER_AGENT}")),
        "{headers}"
    );
    assert!(!lower.contains("x-goog-api-client"));
    assert_eq!(body["project"], "aicode-consumers");
    assert_eq!(body["model"], "gemini-3.8-flash-tiered");
    assert_eq!(
        body["request"]["systemInstruction"]["parts"][0]["text"],
        "be brief"
    );
    assert_eq!(body["request"]["contents"][0]["parts"][0]["text"], "say ok");
    assert_eq!(
        body["request"]["tools"][0]["functionDeclarations"][0]["name"],
        "bash"
    );
    assert!(!dir.path().join("cache/antigravity-token.json").exists());
}

#[tokio::test]
async fn function_call_carries_thought_signature() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "test-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-token",
    );
    let body = array_stream(&[json!({"response": {
        "candidates": [{"content": {"role": "model", "parts": [{
            "thoughtSignature": "sig-call",
            "functionCall": {"name": "bash", "args": {"command": "echo hi"}, "id": "call_1"},
        }]},
        // Deliberate: a call batch is only executed from a candidate that
        // reported a TERMINAL finish reason (B1). An array that closed after a
        // call-shaped prefix used to be accepted; it is an incomplete turn.
        "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 85, "candidatesTokenCount": 15, "thoughtsTokenCount": 37},
    }})]);
    let fake = spawn_fake(1, move |_, _| (200, body.clone())).await;
    let mut model = model_for(&fake, token_file, dir.path().join("cache/token.json"));
    let (assistant, usage) = model
        .complete(
            &[Message::User {
                content: "run echo hi".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .unwrap();
    assert_eq!(assistant.content, None);
    assert_eq!(assistant.tool_calls.len(), 1);
    let call = &assistant.tool_calls[0];
    assert_eq!(call.id, "call_1");
    assert_eq!(call.name, "bash");
    assert_eq!(call.arguments, r#"{"command":"echo hi"}"#);
    assert_eq!(call.provider_meta.as_deref(), Some("sig-call"));
    assert_eq!(usage.unwrap().output_tokens, 52);
}

/// The signature is stored with ITS call and survives a store/load round trip,
/// so the real next POST puts it back at that call's own part position.
#[tokio::test]
async fn signed_call_survives_store_and_load_and_replays_at_its_part_position() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "test-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-token",
    );
    let signature = "SENTINEL-SIGNATURE-FIRST";
    let body = array_stream(&[json!({"response": {
        "candidates": [{"content": {"role": "model", "parts": [{
            "thoughtSignature": signature,
            "functionCall": {"name": "bash", "args": {"command": "echo hi"}, "id": "call_1"},
        }]}, "finishReason": "STOP"}],
    }})]);
    let fake = spawn_fake(2, move |index, _| match index {
        0 => (200, body.clone()),
        _ => (200, array_stream(&[text_element("done")])),
    })
    .await;
    let mut model = model_for(&fake, token_file, dir.path().join("cache/token.json"));
    let user = Message::User {
        content: "run echo hi".into(),
        images: Vec::new(),
    };
    let (assistant, _) = model
        .complete(std::slice::from_ref(&user), &[], None)
        .await
        .unwrap();

    // Store -> load: the call (and its blob) is what a resumed session sees.
    let stored = serde_json::to_string(&Message::Assistant(assistant.clone())).unwrap();
    let reloaded: Message = serde_json::from_str(&stored).unwrap();
    let Message::Assistant(reloaded) = reloaded else {
        panic!("expected the assistant turn back")
    };
    assert_eq!(
        reloaded.tool_calls[0].provider_meta.as_deref(),
        Some(signature)
    );

    let history = vec![
        user,
        Message::Assistant(reloaded),
        Message::Tool {
            call_id: "call_1".into(),
            name: "bash".into(),
            content: "hi\n".into(),
            images: Vec::new(),
            is_error: false,
            synthetic: false,
        },
    ];
    model.complete(&history, &[], None).await.unwrap();

    // The second POST is the real proof: signature beside its own call.
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    let parts = &requests[1].2["request"]["contents"][1]["parts"];
    assert_eq!(parts[0]["functionCall"]["id"], "call_1");
    assert_eq!(parts[0]["thoughtSignature"], signature);
}

/// LIVE-VERIFIED SHAPE: the provider signs the FIRST call of a parallel batch
/// and leaves its sibling unsigned, and the API accepts that on replay. Both
/// calls arrive in the SAME element, which is the only arrangement the old
/// candidate-wide fallback mishandled (it copied the signature onto the
/// sibling), so the assertions below discriminate against it.
#[tokio::test]
async fn parallel_calls_keep_each_signature_at_its_own_part_position() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "test-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-token",
    );
    let body = array_stream(&[json!({"response": {
        "candidates": [{"content": {"role": "model", "parts": [
            {
                "thoughtSignature": "SENTINEL-SIG-A",
                "functionCall": {"name": "bash", "args": {"command": "echo a"}, "id": "call_a"},
            },
            // The sibling, unsigned, in the SAME part list.
            {"functionCall": {"name": "bash", "args": {"command": "echo b"}, "id": "call_b"}},
        ]}, "finishReason": "STOP"}],
    }})]);
    let fake = spawn_fake(2, move |index, _| match index {
        0 => (200, body.clone()),
        _ => (200, array_stream(&[text_element("both done")])),
    })
    .await;
    let mut model = model_for(&fake, token_file, dir.path().join("cache/token.json"));
    let user = Message::User {
        content: "run two commands".into(),
        images: Vec::new(),
    };
    let (assistant, _) = model
        .complete(std::slice::from_ref(&user), &[], None)
        .await
        .unwrap();
    assert_eq!(assistant.tool_calls.len(), 2);
    assert_eq!(
        assistant.tool_calls[0].provider_meta.as_deref(),
        Some("SENTINEL-SIG-A")
    );
    assert_eq!(
        assistant.tool_calls[1].provider_meta, None,
        "the unsigned sibling is not given the batch signature"
    );

    // Store -> load, then the real next POST.
    let stored = serde_json::to_string(&Message::Assistant(assistant)).unwrap();
    let Message::Assistant(reloaded) = serde_json::from_str::<Message>(&stored).unwrap() else {
        panic!("expected the assistant turn back")
    };
    let history = vec![
        user,
        Message::Assistant(reloaded),
        Message::Tool {
            call_id: "call_a".into(),
            name: "bash".into(),
            content: "a\n".into(),
            images: Vec::new(),
            is_error: false,
            synthetic: false,
        },
        Message::Tool {
            call_id: "call_b".into(),
            name: "bash".into(),
            content: "b\n".into(),
            images: Vec::new(),
            is_error: false,
            synthetic: false,
        },
    ];
    model.complete(&history, &[], None).await.unwrap();
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    let parts = &requests[1].2["request"]["contents"][1]["parts"];
    assert_eq!(parts[0]["functionCall"]["id"], "call_a");
    assert_eq!(parts[0]["thoughtSignature"], "SENTINEL-SIG-A");
    assert_eq!(parts[1]["functionCall"]["id"], "call_b");
    assert!(
        parts[1].get("thoughtSignature").is_none(),
        "the sibling must stay unsigned: {parts:?}"
    );
}

/// H6: a tool history with no signature anywhere is the verified 400 case, so
/// it fails locally with the offending call instead of borrowing a signature
/// or spending a doomed request.
#[tokio::test]
async fn signature_less_tool_history_is_refused_locally() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "test-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-token",
    );
    let fake = spawn_fake(1, move |_, _| (200, array_stream(&[text_element("ok")]))).await;
    let mut model = model_for(&fake, token_file, dir.path().join("cache/token.json"));
    let history = vec![
        Message::User {
            content: "run".into(),
            images: Vec::new(),
        },
        Message::Assistant(AssistantMessage {
            content: None,
            tool_calls: vec![
                ToolCall {
                    id: "call_1".into(),
                    name: "bash".into(),
                    arguments: r#"{"command":"ls"}"#.into(),
                    provider_meta: None,
                },
                ToolCall {
                    id: "call_2".into(),
                    name: "bash".into(),
                    arguments: r#"{"command":"pwd"}"#.into(),
                    provider_meta: None,
                },
            ],
            reasoning: None,
        }),
        // The verified 400 shape: the open batch is the one whose results are
        // being sent, so they are part of the history.
        Message::Tool {
            call_id: "call_1".into(),
            name: "bash".into(),
            content: "a.txt".into(),
            images: Vec::new(),
            is_error: false,
            synthetic: false,
        },
        Message::Tool {
            call_id: "call_2".into(),
            name: "bash".into(),
            content: "/tmp".into(),
            images: Vec::new(),
            is_error: false,
            synthetic: false,
        },
    ];
    let error = model
        .complete(&history, &[], None)
        .await
        .expect_err("a signature-less open batch is refused");
    let message = format!("{error:#}");
    // The ordinal, never the provider-supplied id or name: the runner persists
    // this text (B4).
    assert!(message.contains("1 of 2"), "{message}");
    assert!(message.contains("thoughtSignature"), "{message}");
    assert!(!message.contains("call_1"), "{message}");
    assert!(!message.contains("call_2"), "{message}");
    assert_eq!(fake.requests().len(), 0, "no doomed request is sent");
}

/// F1: replacement and conditional eviction are SERIALIZED, so a refresh that
/// installs a newer entry can never be deleted by a rejection of the older
/// credential. The test parks a real eviction behind the cache lock, installs
/// the newer entry meanwhile (as another refresh's rename would), then lets the
/// eviction run: it re-reads under the lock, sees a different credential, and
/// leaves it alone.
#[tokio::test]
async fn eviction_never_deletes_a_newer_entry() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "expired-token",
        Utc::now() - ChronoDuration::minutes(5),
        "cli-refresh",
    );
    let cache_file = dir.path().join("cache/antigravity-token.json");
    write_cache(
        &cache_file,
        "expired-cached-token",
        Utc::now() - ChronoDuration::minutes(5),
        "rejected-refresh",
        "cli-refresh",
    );
    let fake = spawn_fake(1, move |_, _| {
        (
            400,
            json!({"error": "invalid_grant", "error_description": "expired"}).to_string(),
        )
    })
    .await;

    // Hold the cache lock: the eviction cannot proceed past it, which is the
    // window the fix closes.
    let guard = CACHE_LOCK.lock().await;
    let mut model = model_for(&fake, token_file, cache_file.clone());
    let rejection = tokio::spawn(async move {
        model
            .complete(
                &[Message::User {
                    content: "say ok".into(),
                    images: Vec::new(),
                }],
                &[],
                None,
            )
            .await
            .map(|_| ())
    });
    // Let the request (which is NOT behind the lock) reach the rejection and
    // park at the eviction.
    for _ in 0..128 {
        tokio::task::yield_now().await;
        if !fake.requests().is_empty() {
            break;
        }
    }
    assert_eq!(fake.requests().len(), 1, "the rejection came back");
    // The rejection is known to the task now, so an eviction that ignored the
    // lock would already have deleted the entry: it must still be here.
    for _ in 0..128 {
        tokio::task::yield_now().await;
    }
    assert!(
        cache_file.exists(),
        "the eviction ran outside the cache lock"
    );
    write_cache(
        &cache_file,
        "newer-access-token",
        Utc::now() + ChronoDuration::hours(1),
        "newer-refresh",
        "cli-refresh",
    );
    drop(guard);

    let error = rejection
        .await
        .unwrap()
        .expect_err("the rejected turn still fails");
    assert!(format!("{error:#}").contains("invalid_grant"), "{error:#}");
    let survivor = super::read_token(&cache_file)
        .expect("the newer entry is readable")
        .expect("the newer entry survived the eviction");
    assert_eq!(
        survivor.token.refresh_token.as_deref(),
        Some("newer-refresh")
    );
}

/// F1 (other half): replacement takes the same lock, so it cannot land in the
/// middle of a conditional eviction either. The write must wait for the lock
/// and must not touch the file before it has it.
#[tokio::test]
async fn replacement_waits_for_the_cache_lock() {
    let dir = tempfile::tempdir().unwrap();
    let cache_file = dir.path().join("cache/antigravity-token.json");
    let guard = CACHE_LOCK.lock().await;
    let waiting = tokio::spawn({
        let cache_file = cache_file.clone();
        async move {
            replace_cache(
                &cache_file,
                &TokenFile {
                    token: StoredToken {
                        access_token: "fresh-token".into(),
                        refresh_token: Some("fresh-refresh".into()),
                        expiry: Some(Utc::now() + ChronoDuration::hours(1)),
                    },
                    cli_fingerprint: Some(login_fingerprint("cli-refresh")),
                },
            )
            .await
        }
    });
    for _ in 0..128 {
        tokio::task::yield_now().await;
    }
    assert!(
        !cache_file.exists(),
        "the replacement wrote the cache outside the lock"
    );
    drop(guard);
    waiting.await.unwrap().expect("the replacement writes");
    assert!(cache_file.exists(), "the replacement landed");
}

/// Z2: eviction is scoped to the credential that was actually rejected. Here
/// the refresh used the CLI's own credential (the cache holds no refresh
/// token), so the cache must survive the rejection.
#[tokio::test]
async fn invalid_grant_keeps_a_cache_it_did_not_reject() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "expired-token",
        Utc::now() - ChronoDuration::minutes(5),
        "cli-refresh",
    );
    let cache_file = dir.path().join("cache/antigravity-token.json");
    std::fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
    // Bound to this login, but with no refresh credential of its own.
    std::fs::write(
        &cache_file,
        serde_json::to_vec(&json!({
            "token": {
                "access_token": "expired-cached-token",
                "expiry": (Utc::now() - ChronoDuration::minutes(5)).to_rfc3339(),
            },
            "cli_fingerprint": login_fingerprint("cli-refresh"),
        }))
        .unwrap(),
    )
    .unwrap();
    let fake = spawn_fake(1, move |_, _| {
        (
            400,
            json!({"error": "invalid_grant", "error_description": "Token has been expired or revoked."})
                .to_string(),
        )
    })
    .await;
    let mut model = model_for(&fake, token_file, cache_file.clone());
    let error = model
        .complete(
            &[Message::User {
                content: "say ok".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .expect_err("the CLI credential was rejected");
    assert!(format!("{error:#}").contains("invalid_grant"), "{error:#}");
    let form = &fake.raw_bodies()[0];
    assert!(form.contains("refresh_token=cli-refresh"), "{form}");
    assert!(
        cache_file.exists(),
        "a cache entry this rejection did not cover must survive"
    );
}

/// Z1: a batch a later assistant answer has already closed is not "active".
/// `user → unsigned call → tool result → final answer → new user prompt` must
/// produce the request; the old guard selected that closed and unsigned batch
/// and refused the new prompt locally, which broke resumed/forked sessions.
#[tokio::test]
async fn closed_tool_batch_does_not_block_a_new_turn() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "test-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-token",
    );
    let fake = spawn_fake(1, move |_, _| (200, array_stream(&[text_element("ok")]))).await;
    let mut model = model_for(&fake, token_file, dir.path().join("cache/token.json"));
    let history = vec![
        Message::User {
            content: "run ls".into(),
            images: Vec::new(),
        },
        // Old history written by another model: its call has no signature.
        Message::Assistant(AssistantMessage {
            content: None,
            tool_calls: vec![ToolCall {
                id: "old_call".into(),
                name: "bash".into(),
                arguments: r#"{"command":"ls"}"#.into(),
                provider_meta: None,
            }],
            reasoning: None,
        }),
        Message::Tool {
            call_id: "old_call".into(),
            name: "bash".into(),
            content: "a.txt".into(),
            images: Vec::new(),
            is_error: false,
            synthetic: false,
        },
        // ...and the model's final answer CLOSES that batch.
        Message::Assistant(AssistantMessage {
            content: Some("there is a.txt".into()),
            tool_calls: Vec::new(),
            reasoning: None,
        }),
        // A new user prompt: nothing is open, so nothing to validate.
        Message::User {
            content: "thanks, now say ok".into(),
            images: Vec::new(),
        },
    ];
    let (assistant, _) = model
        .complete(&history, &[], None)
        .await
        .expect("a new turn after a closed batch must reach the provider");
    assert_eq!(assistant.content.as_deref(), Some("ok"));
    let requests = fake.requests();
    assert_eq!(requests.len(), 1, "exactly one request was sent");
    let contents = &requests[0].2["request"]["contents"];
    assert_eq!(
        contents[1]["parts"][0]["functionCall"]["id"], "old_call",
        "the closed batch replays as recorded: {contents:?}"
    );
    assert!(
        contents[1]["parts"][0].get("thoughtSignature").is_none(),
        "no signature is fabricated for old history: {contents:?}"
    );
    assert_eq!(contents[4]["parts"][0]["text"], "thanks, now say ok");
}

/// B5: `invalid_grant` is definitive. The dead cache entry is removed so the
/// NEXT request starts over from the CLI credential, and the error says what to
/// do. No extra retry happens inside the failed turn.
#[tokio::test]
async fn invalid_grant_clears_the_dead_cache_and_uses_the_cli_credential_next() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "expired-token",
        Utc::now() - ChronoDuration::minutes(5),
        "cli-refresh",
    );
    let cache_file = dir.path().join("cache/antigravity-token.json");
    // A bound cache holding a rotated refresh token that the endpoint now
    // rejects: same login, dead credential.
    write_cache(
        &cache_file,
        "expired-cached-token",
        Utc::now() - ChronoDuration::minutes(5),
        "rotated-refresh",
        "cli-refresh",
    );
    let body = array_stream(&[text_element("ok")]);
    let fake = spawn_fake(3, move |index, path| match (index, path) {
        (0, "/token") => (
            400,
            json!({"error": "invalid_grant", "error_description": "Token has been expired or revoked."})
                .to_string(),
        ),
        (1, "/token") => (
            200,
            json!({"access_token": "fresh-token", "expires_in": 3599}).to_string(),
        ),
        _ => (200, body.clone()),
    })
    .await;
    let mut model = model_for(&fake, token_file, cache_file.clone());
    let error = model
        .complete(
            &[Message::User {
                content: "say ok".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .expect_err("a dead cached credential fails the turn");
    let message = format!("{error:#}");
    assert!(message.contains("invalid_grant"), "{message}");
    assert!(message.contains("agy"), "{message}");
    assert!(
        !cache_file.exists(),
        "the dead cache entry is gone: {message}"
    );

    // The next turn starts from the CLI credential, not from the dead one.
    model
        .complete(
            &[Message::User {
                content: "say ok".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .expect("the CLI credential still works");
    let form = &fake.raw_bodies()[1];
    assert!(form.contains("refresh_token=cli-refresh"), "{form}");
    assert!(!form.contains("rotated-refresh"), "{form}");
    assert_eq!(
        super::read_token(&cache_file)
            .unwrap()
            .unwrap()
            .token
            .access_token,
        "fresh-token"
    );
}

/// H1: an element split at EVERY byte boundary — an escaped Unicode escape
/// included — still decodes, and the framing is only accepted at EOF.
/// B2(d): only the batch being continued NOW needs its signature. An older,
/// entirely unsigned tool round is replayed as the record has it (a session
/// resumed on this model from another provider must still work).
#[tokio::test]
async fn unsigned_old_history_is_replayed_as_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "test-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-token",
    );
    let fake = spawn_fake(1, move |_, _| (200, array_stream(&[text_element("done")]))).await;
    let mut model = model_for(&fake, token_file, dir.path().join("cache/token.json"));
    let history = vec![
        Message::User {
            content: "old request".into(),
            images: Vec::new(),
        },
        // Old history: no signature at all.
        Message::Assistant(AssistantMessage {
            content: None,
            tool_calls: vec![ToolCall {
                id: "old_call".into(),
                name: "bash".into(),
                arguments: r#"{"command":"ls"}"#.into(),
                provider_meta: None,
            }],
            reasoning: None,
        }),
        Message::Tool {
            call_id: "old_call".into(),
            name: "bash".into(),
            content: "a.txt".into(),
            images: Vec::new(),
            is_error: false,
            synthetic: false,
        },
        // The ACTIVE batch: signed, so the step being continued is valid.
        Message::Assistant(AssistantMessage {
            content: None,
            tool_calls: vec![ToolCall {
                id: "now_call".into(),
                name: "bash".into(),
                arguments: r#"{"command":"pwd"}"#.into(),
                provider_meta: Some("SENTINEL-SIG-NOW".into()),
            }],
            reasoning: None,
        }),
        Message::Tool {
            call_id: "now_call".into(),
            name: "bash".into(),
            content: "/tmp".into(),
            images: Vec::new(),
            is_error: false,
            synthetic: false,
        },
    ];
    model
        .complete(&history, &[], None)
        .await
        .expect("old unsigned history must not block the turn");
    let parts = &fake.requests()[0].2["request"]["contents"];
    assert_eq!(parts[1]["parts"][0]["functionCall"]["id"], "old_call");
    assert!(
        parts[1]["parts"][0].get("thoughtSignature").is_none(),
        "the old call is replayed as recorded: {parts:?}"
    );
    assert_eq!(parts[3]["parts"][0]["thoughtSignature"], "SENTINEL-SIG-NOW");
}

/// B2: a visible part keeps its text (and its delta) even when the provider
/// also stamped a `thoughtSignature` on it — it is NOT a signature-only part.
#[tokio::test]
async fn signed_text_part_keeps_text_and_delta() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "test-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-token",
    );
    let body = array_stream(&[json!({"response": {
        "candidates": [{"content": {"role": "model", "parts": [
            {"text": "the answer", "thoughtSignature": "SENTINEL-SIG-TEXT"},
        ]}, "finishReason": "STOP"}],
    }})]);
    let fake = spawn_fake(1, move |_, _| (200, body.clone())).await;
    let mut model = model_for(&fake, token_file, dir.path().join("cache/token.json"));
    let mut deltas: Vec<(ModelDeltaKind, String)> = Vec::new();
    let mut sink = |kind: ModelDeltaKind, text: &str| deltas.push((kind, text.to_owned()));
    let (assistant, _) = model
        .complete(
            &[Message::User {
                content: "say the answer".into(),
                images: Vec::new(),
            }],
            &[],
            Some(&mut sink),
        )
        .await
        .expect("a signed text part is still a full answer");
    assert_eq!(assistant.content.as_deref(), Some("the answer"));
    assert_eq!(
        deltas,
        [(ModelDeltaKind::Content, "the answer".to_owned())],
        "the delta must reach the caller"
    );
    assert!(assistant.tool_calls.is_empty());
}

/// B2: a signature-only part DIRECTLY after its call signs that call.
#[tokio::test]
async fn signature_only_part_after_a_call_attaches() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "test-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-token",
    );
    let body = array_stream(&[json!({"response": {
        "candidates": [{"content": {"role": "model", "parts": [
            {"functionCall": {"name": "bash", "args": {"command": "echo a"}, "id": "call_a"}},
            {"thoughtSignature": "SENTINEL-SIG-LATE", "text": ""},
        ]}, "finishReason": "STOP"}],
    }})]);
    let fake = spawn_fake(1, move |_, _| (200, body.clone())).await;
    let mut model = model_for(&fake, token_file, dir.path().join("cache/token.json"));
    let (assistant, _) = model
        .complete(
            &[Message::User {
                content: "run".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .unwrap();
    assert_eq!(assistant.tool_calls.len(), 1);
    assert_eq!(
        assistant.tool_calls[0].provider_meta.as_deref(),
        Some("SENTINEL-SIG-LATE")
    );
    assert_eq!(
        assistant.content, None,
        "the trailing empty text is not content"
    );
}

/// F2: a part with nonempty provider-thought text is TEXT even when it also
/// carries a `thoughtSignature` — its delta is emitted and the signature never
/// signs the call before it. On the previous code the thought text was
/// filtered out of the "visible text" test, so the part looked signature-only
/// and stamped the preceding call. B3 makes non-attachment OBSERVABLE: the call
/// stays unsigned, so the batch is refused instead of run (a borrowed
/// signature would have satisfied the check and returned `Ok`).
#[tokio::test]
async fn thought_text_with_a_signature_does_not_sign_the_call() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "test-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-token",
    );
    let body = array_stream(&[json!({"response": {
        "candidates": [{"content": {"role": "model", "parts": [
            {"functionCall": {"name": "bash", "args": {"command": "echo a"}, "id": "call_a"}},
            {
                "text": "thinking about the command",
                "thought": true,
                "thoughtSignature": "SENTINEL-SIG-THOUGHT",
            },
        ]}, "finishReason": "STOP"}],
    }})]);
    let fake = spawn_fake(1, move |_, _| (200, body.clone())).await;
    let mut model = model_for(&fake, token_file, dir.path().join("cache/token.json"));
    let mut deltas: Vec<(ModelDeltaKind, String)> = Vec::new();
    let mut sink = |kind: ModelDeltaKind, text: &str| deltas.push((kind, text.to_owned()));
    let error = model
        .complete(
            &[Message::User {
                content: "run".into(),
                images: Vec::new(),
            }],
            &[],
            Some(&mut sink),
        )
        .await
        .expect_err("thinking text is text: its signature must not sign the call");
    let message = format!("{error:#}");
    assert!(
        message.contains("no thoughtSignature"),
        "the call is still unsigned: {message}"
    );
    assert!(!message.contains("thinking about the command"), "{message}");
    assert_eq!(
        deltas,
        [(
            ModelDeltaKind::Reasoning,
            "thinking about the command".to_owned()
        )],
        "the thinking text is still emitted"
    );
}

/// Z3: an EMPTY text part is still an intervening part: a signature-only part
/// after it does not attach backwards to the call before it, so that call stays
/// unsigned and (B3) the batch is refused rather than run.
#[tokio::test]
async fn empty_text_part_breaks_the_signature_adjacency() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "test-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-token",
    );
    let body = array_stream(&[json!({"response": {
        "candidates": [{"content": {"role": "model", "parts": [
            {"functionCall": {"name": "bash", "args": {"command": "echo a"}, "id": "call_a"}},
            {"text": ""},
            {"thoughtSignature": "SENTINEL-SIG-AFTER-EMPTY"},
        ]}, "finishReason": "STOP"}],
    }})]);
    let fake = spawn_fake(1, move |_, _| (200, body.clone())).await;
    let mut model = model_for(&fake, token_file, dir.path().join("cache/token.json"));
    let error = model
        .complete(
            &[Message::User {
                content: "run".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .expect_err("the empty text part broke the adjacency, so the call is unsigned");
    let message = format!("{error:#}");
    assert!(
        message.contains("no thoughtSignature"),
        "the late signature did not reach the call: {message}"
    );
    assert!(!message.contains("SENTINEL-SIG-AFTER-EMPTY"), "{message}");
}

/// B2: an intervening text part breaks the adjacency — a signature part that
/// follows it is not attached backwards to the call, so that call stays
/// unsigned and (B3) the batch is refused rather than run.
#[tokio::test]
async fn signature_only_part_after_intervening_text_does_not_attach() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "test-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-token",
    );
    let body = array_stream(&[json!({"response": {
        "candidates": [{"content": {"role": "model", "parts": [
            {"functionCall": {"name": "bash", "args": {"command": "echo a"}, "id": "call_a"}},
            {"text": "let me check"},
            {"thoughtSignature": "SENTINEL-SIG-FAR"},
        ]}, "finishReason": "STOP"}],
    }})]);
    let fake = spawn_fake(1, move |_, _| (200, body.clone())).await;
    let mut model = model_for(&fake, token_file, dir.path().join("cache/token.json"));
    let mut deltas: Vec<(ModelDeltaKind, String)> = Vec::new();
    let mut sink = |kind: ModelDeltaKind, text: &str| deltas.push((kind, text.to_owned()));
    let error = model
        .complete(
            &[Message::User {
                content: "run".into(),
                images: Vec::new(),
            }],
            &[],
            Some(&mut sink),
        )
        .await
        .expect_err("a non-adjacent signature must not be borrowed");
    let message = format!("{error:#}");
    assert!(
        message.contains("no thoughtSignature"),
        "the call is still unsigned: {message}"
    );
    assert!(!message.contains("SENTINEL-SIG-FAR"), "{message}");
    assert_eq!(
        deltas,
        [(ModelDeltaKind::Content, "let me check".to_owned())],
        "the intervening text is still content"
    );
}

/// The tool-result request body: the assistant turn echoes the original
/// `functionCall` together with its `thoughtSignature`, and the result is a
/// `functionResponse` naming the same id.
#[test]
fn tool_result_body_echoes_signature_and_function_response() {
    let dir = tempfile::tempdir().unwrap();
    let model = AntigravityModel {
        client: reqwest::Client::new(),
        model: "gemini-3.8-flash-tiered".into(),
        reasoning_effort: None,
        stream_url: "http://127.0.0.1:1/stream".into(),
        token_endpoint: "http://127.0.0.1:1/token".into(),
        token_file: dir.path().join("agy.json"),
        cache_file: dir.path().join("cache.json"),
        client_id: None,
        client_secret: None,
        min_interval: std::time::Duration::ZERO,
        last_request: Default::default(),
    };
    let messages = vec![
        Message::User {
            content: "run echo hi".into(),
            images: Vec::new(),
        },
        Message::Assistant(AssistantMessage {
            content: None,
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                name: "bash".into(),
                arguments: r#"{"command":"echo hi"}"#.into(),
                provider_meta: Some("sig-call".into()),
            }],
            reasoning: None,
        }),
        Message::Tool {
            call_id: "call_1".into(),
            name: "bash".into(),
            content: "hi\n".into(),
            images: Vec::new(),
            is_error: false,
            synthetic: false,
        },
    ];
    let body = model.request_body(&messages, &[]).unwrap();
    let contents = body["request"]["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 3);
    assert_eq!(contents[1]["role"], "model");
    assert_eq!(
        contents[1]["parts"][0]["functionCall"],
        json!({"name": "bash", "args": {"command": "echo hi"}, "id": "call_1"})
    );
    assert_eq!(contents[1]["parts"][0]["thoughtSignature"], "sig-call");
    assert_eq!(contents[2]["role"], "user");
    assert_eq!(
        contents[2]["parts"][0]["functionResponse"],
        json!({"name": "bash", "id": "call_1", "response": {"output": "hi\n"}})
    );
}

#[tokio::test]
async fn unauthorized_refreshes_once_then_retries() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "stale-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-token",
    );
    let cache_file = dir.path().join("cache/antigravity-token.json");
    let cli_bytes_before = std::fs::read(&token_file).unwrap();
    let body = array_stream(&[text_element("ok")]);
    // Six connections: 401 -> refresh -> retry (ok), then a SECOND turn whose
    // refreshed token is rejected too: 401 -> refresh -> 401 -> stop.
    let fake = spawn_fake(6, move |index, path| match (index, path) {
        (1 | 4, "/token") => (
            200,
            json!({"access_token": format!("fresh-token-{index}"), "expires_in": 3599}).to_string(),
        ),
        (2, _) => (200, body.clone()),
        _ => (
            401,
            json!([{"error": {"code": 401, "status": "UNAUTHENTICATED"}}]).to_string(),
        ),
    })
    .await;
    let mut model = model_for(&fake, token_file.clone(), cache_file.clone());
    let (assistant, _) = model
        .complete(
            &[Message::User {
                content: "say ok".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .unwrap();
    assert_eq!(assistant.content.as_deref(), Some("ok"));
    let requests = fake.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[1].0.ends_with("/token"));
    assert!(
        requests[1]
            .1
            .to_ascii_lowercase()
            .contains("content-type: application/x-www-form-urlencoded")
    );
    // The retry spends the refreshed token.
    assert!(
        requests[2]
            .1
            .to_ascii_lowercase()
            .contains("authorization: bearer fresh-token-1")
    );
    // The refresh form is exactly the configured consumer client exchanging
    // the CLI login's own refresh token.
    let form = &fake.raw_bodies()[1];
    for field in [
        format!("client_id={TEST_CLIENT_ID}"),
        format!("client_secret={TEST_CLIENT_SECRET}"),
        "refresh_token=refresh-token".to_owned(),
        "grant_type=refresh_token".to_owned(),
    ] {
        assert!(form.contains(&field), "{form}");
    }
    // The refresh is cached in e-agent's own dir (agy's file is untouched).
    let cached = super::read_token(&cache_file).unwrap().unwrap();
    assert_eq!(cached.token.access_token, "fresh-token-1");
    assert_eq!(cached.token.refresh_token.as_deref(), Some("refresh-token"));
    assert_eq!(
        cached.cli_fingerprint.as_deref(),
        Some(login_fingerprint("refresh-token").as_str()),
        "the cache is bound to the CLI login that produced it"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&cache_file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    // agy's file is read-only for e-agent: byte-identical after the refresh.
    assert_eq!(std::fs::read(&token_file).unwrap(), cli_bytes_before);
    assert_eq!(
        fake.raw_bodies()
            .iter()
            .filter(|body| body.contains("grant_type=refresh_token"))
            .count(),
        1,
        "the first turn refreshed exactly once"
    );

    // A SECOND 401 (even after a successful refresh) stops: one refresh plus
    // one retry for that turn, then the error. No third generation attempt.
    let error = model
        .complete(
            &[Message::User {
                content: "say ok again".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .expect_err("a second 401 is a failure, not another retry");
    let message = format!("{error:#}");
    assert!(message.contains("401"), "{message}");
    assert!(message.contains("UNAUTHENTICATED"), "{message}");
    let requests = fake.requests();
    assert_eq!(
        requests.len(),
        6,
        "the second turn made one generation attempt, one refresh and one retry"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|(path, _, _)| path.ends_with("/token"))
            .count(),
        2,
        "one refresh per turn, never a loop"
    );
}

#[tokio::test]
async fn expired_token_refreshes_before_the_request() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "expired-token",
        Utc::now() - ChronoDuration::minutes(5),
        "refresh-token",
    );
    let body = array_stream(&[text_element("ok")]);
    let fake = spawn_fake(2, move |index, _| match index {
        0 => (
            200,
            json!({"access_token": "fresh-token", "expires_in": 3599}).to_string(),
        ),
        _ => (200, body.clone()),
    })
    .await;
    let mut model = model_for(&fake, token_file, dir.path().join("cache/token.json"));
    model
        .complete(
            &[Message::User {
                content: "say ok".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .unwrap();
    let requests = fake.requests();
    assert_eq!(requests.len(), 2, "one refresh, then the request");
    assert!(requests[0].0.ends_with("/token"));
    assert!(
        requests[1]
            .1
            .to_ascii_lowercase()
            .contains("authorization: bearer fresh-token")
    );
}

#[tokio::test]
async fn valid_token_is_reused_without_refreshing() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "valid-token",
        Utc::now() + ChronoDuration::minutes(50),
        "refresh-token",
    );
    let body = array_stream(&[text_element("ok")]);
    let fake = spawn_fake(2, move |_, _| (200, body.clone())).await;
    let mut model = model_for(&fake, token_file, dir.path().join("cache/token.json"));
    for _ in 0..2 {
        model
            .complete(
                &[Message::User {
                    content: "say ok".into(),
                    images: Vec::new(),
                }],
                &[],
                None,
            )
            .await
            .unwrap();
    }
    let requests = fake.requests();
    assert_eq!(requests.len(), 2, "two requests, no token refresh");
    for (path, headers, _) in requests {
        assert!(path.ends_with(":streamGenerateContent"), "{path}");
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("authorization: bearer valid-token")
        );
    }
}

/// Without `client_id`/`client_secret` the provider still works while `agy`'s
/// access token is valid: no refresh happens, so the configured consumer
/// client is never needed.
#[tokio::test]
async fn valid_cli_token_needs_no_client_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "valid-token",
        Utc::now() + ChronoDuration::minutes(50),
        "refresh-token",
    );
    let body = array_stream(&[text_element("ok")]);
    let fake = spawn_fake(1, move |_, _| (200, body.clone())).await;
    let mut model = model_for(&fake, token_file, dir.path().join("cache/token.json"));
    model.client_id = None;
    model.client_secret = None;
    let (assistant, _) = model
        .complete(
            &[Message::User {
                content: "say ok".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .unwrap();
    assert_eq!(assistant.content.as_deref(), Some("ok"));
    let requests = fake.requests();
    assert_eq!(requests.len(), 1, "no refresh, so no token request");
    assert!(
        requests[0]
            .1
            .to_ascii_lowercase()
            .contains("authorization: bearer valid-token")
    );
}

/// An expired token with no configured consumer client is a config error
/// naming the missing keys — before any request, and without panicking.
#[tokio::test]
async fn expired_token_without_client_credentials_names_the_config_keys() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "expired-token",
        Utc::now() - ChronoDuration::minutes(5),
        "refresh-token",
    );
    // Deliberately zero connections: the refresh must fail before the wire.
    let fake = spawn_fake(0, move |_, _| (500, String::new())).await;
    let mut model = model_for(&fake, token_file, dir.path().join("cache/token.json"));
    model.client_id = None;
    model.client_secret = None;
    let error = model
        .complete(
            &[Message::User {
                content: "say ok".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .expect_err("a refresh without the consumer client cannot proceed");
    let message = format!("{error:#}");
    assert!(
        message.contains("client_id") && message.contains("client_secret"),
        "{message}"
    );
    assert!(message.contains("`agy` install"), "{message}");
    assert!(
        message.contains("still-valid access token"),
        "the message must say a valid token works without the keys: {message}"
    );
    assert!(
        fake.requests().is_empty(),
        "no request may be sent: {message}"
    );
}

/// `agy` writes nanosecond precision with a local UTC offset; the parse must
/// keep the absolute instant and the usability window must follow the clock.
#[test]
fn cli_token_expiry_is_parsed_and_time_relative() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("agy.json");
    std::fs::write(
        &path,
        br#"{"token":{"access_token":"a","refresh_token":"r","expiry":"2026-11-23T20:30:38.606537348+08:00","token_type":"Bearer"}}"#,
    )
    .unwrap();
    let token = super::read_token(&path).unwrap().unwrap().token;
    assert_eq!(token.access_token, "a");
    assert_eq!(token.refresh_token.as_deref(), Some("r"));
    assert_eq!(
        token.expiry,
        Some(
            "2026-11-23T20:30:38.606537348+08:00"
                .parse::<DateTime<Utc>>()
                .unwrap()
        )
    );
    let usable = |expiry: Option<DateTime<Utc>>| {
        StoredToken {
            access_token: "a".into(),
            refresh_token: None,
            expiry,
        }
        .usable()
    };
    assert!(usable(Some(Utc::now() + ChronoDuration::hours(1))));
    assert!(!usable(Some(Utc::now() - ChronoDuration::seconds(5))));
    assert!(usable(None), "an unknown expiry defers to the API's 401");
}

#[tokio::test]
async fn array_stream_reassembles_split_elements() {
    let element = serde_json::to_string(&text_element("hello")).unwrap();
    let body = format!("[\n{element}\n,\n{element}\n]");
    let mut scanner = ArrayStream::default();
    let mut elements = Vec::new();
    for chunk in body.as_bytes().chunks(17) {
        elements.extend(scanner.push(chunk).unwrap());
    }
    scanner.finish().unwrap();
    assert_eq!(elements.len(), 2);
    assert_eq!(
        elements[1]["response"]["candidates"][0]["content"]["parts"][0]["text"],
        "hello"
    );
}
#[test]
fn array_stream_decodes_a_byte_at_a_time_escaped_unicode_element() {
    // Raw JSON text, so the wire bytes really carry \uXXXX escapes (a
    // `json!` literal would arrive already decoded).
    let body = r#"[{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"caf\u00e9 \ud83d\ude00"}]}}]}}]"#;
    assert!(body.contains(r"\u00e9"), "{body}");
    assert!(body.contains(r"\ud83d"), "{body}");
    let mut scanner = ArrayStream::default();
    let mut elements = Vec::new();
    for byte in body.as_bytes() {
        elements.extend(scanner.push(std::slice::from_ref(byte)).unwrap());
    }
    assert!(
        scanner.finish().is_ok(),
        "a complete array must be accepted at EOF"
    );
    assert_eq!(
        elements[0]["response"]["candidates"][0]["content"]["parts"][0]["text"],
        "caf\u{e9} \u{1f600}"
    );
}

/// H1: the framing rules that make a short read an error. Each case is
/// something the service could send when a stream is cut.
#[test]
fn array_stream_rejects_bad_framing() {
    let element = serde_json::to_string(&text_element("hello")).unwrap();
    let truncated_second = format!("[\n{element}\n,\n{{\"response\": {{\"candidates\": [");
    const FORM_FEED: char = '\u{0c}';
    let cases = [
        // A cut mid-object: the prefix's own element decoded already.
        truncated_second,
        // Missing the closing `]`.
        format!("[\n{element}\n"),
        // Trailing garbage after the array.
        format!("[\n{element}\n]\nnot-json"),
        // A leading separator / missing `[`.
        format!("\n{element}\n]"),
        // A trailing comma: after `,` an element (never `]`) must come.
        format!("[{element},]"),
        // A double close.
        format!("[{element}]]"),
        // A leading separator.
        format!("[,{element}]"),
        // A form feed is not JSON whitespace, at the tail...
        format!("[{element}]{}", FORM_FEED),
        // ...nor inside the framing.
        format!("[{FORM_FEED}{element}]"),
    ];
    for body in &cases {
        let mut scanner = ArrayStream::default();
        let error = scanner
            .push(body.as_bytes())
            .and_then(|_| scanner.finish())
            .expect_err(&format!("must be rejected: {body}"));
        assert!(!format!("{error:#}").is_empty());
    }
    // The accepting neighbours: a single-element array, and an EOF immediately
    // after `]` with (and without) JSON whitespace around the framing.
    for body in [
        format!("[{element}]"),
        format!("[\n{element}\n]"),
        format!("[\t{element}\r\n,\r\n{element}\t]"),
    ] {
        let mut scanner = ArrayStream::default();
        let elements = scanner.push(body.as_bytes()).expect(body.as_str());
        scanner.finish().expect(body.as_str());
        assert!(!elements.is_empty(), "{body}");
    }
}

/// H1 end-to-end: a clean EOF in the middle of an element is NOT a
/// successful turn, even though the prefix carried a usable-looking call.
#[tokio::test]
async fn truncated_stream_is_not_a_successful_turn() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "test-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-token",
    );
    let first = json!({"response": {"candidates": [{"content": {"role": "model", "parts": [{
        "thoughtSignature": "sig-call",
        "functionCall": {"name": "bash", "args": {"command": "echo hi"}, "id": "call_1"},
    }]}, "finishReason": "STOP"}], "usageMetadata": {"promptTokenCount": 3}}});
    let body = format!(
        "[\n{}\n,\n{{\"response\": {{\"candidates\": [{{\"content\": ",
        serde_json::to_string(&first).unwrap()
    );
    let fake = spawn_fake(1, move |_, _| (200, body.clone())).await;
    let mut model = model_for(&fake, token_file, dir.path().join("cache/token.json"));
    let error = model
        .complete(
            &[Message::User {
                content: "run echo hi".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .expect_err("a truncated array must not become a turn");
    let message = format!("{error:#}");
    assert!(message.contains("truncated"), "{message}");
}

/// B1: a call-shaped prefix is not a completed generation. The candidate that
/// carries the calls must terminate too, and only a SUCCESSFUL terminal may
/// hand executable calls back: `MALFORMED_FUNCTION_CALL` / `SAFETY` parts are
/// fragments of a generation the provider itself rejected.
#[tokio::test]
async fn failure_finish_after_a_valid_call_prefix_is_not_a_turn() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "test-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-token",
    );
    for failure in ["MALFORMED_FUNCTION_CALL", "SAFETY"] {
        let body = array_stream(&[
            // A complete, signed, replayable-looking call...
            json!({"response": {"candidates": [{"content": {"role": "model", "parts": [{
                "thoughtSignature": "sig-call",
                "functionCall": {"name": "bash", "args": {"command": "rm -rf /"}, "id": "call_1"},
            }]}}]}}),
            // ...followed by the terminal candidate the provider rejected.
            json!({"response": {"candidates": [{"content": {"role": "model", "parts": []},
                "finishReason": failure}]}}),
        ]);
        let fake = spawn_fake(1, move |_, _| (200, body.clone())).await;
        let mut model = model_for(
            &fake,
            token_file.clone(),
            dir.path().join("cache/token.json"),
        );
        let error = model
            .complete(
                &[Message::User {
                    content: "run".into(),
                    images: Vec::new(),
                }],
                &[],
                None,
            )
            .await
            .expect_err("a failed candidate must not return executable calls");
        let message = format!("{error:#}");
        assert!(message.contains(failure), "{message}");
    }

    // An unrecognized finish value is provider-controlled text: it is not
    // evidence of success and is never echoed into a persisted message.
    const SENTINEL: &str = "SENTINEL-FINISH-9f2c";
    let unknown = array_stream(&[json!({"response": {"candidates": [{
        "content": {"role": "model", "parts": [{"text": "ok"}]},
        "finishReason": SENTINEL,
    }]}})]);
    let fake = spawn_fake(1, move |_, _| (200, unknown.clone())).await;
    let mut model = model_for(&fake, token_file, dir.path().join("cache/token.json"));
    let error = model
        .complete(
            &[Message::User {
                content: "say ok".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .expect_err("an unrecognized finish is not a completed turn");
    let message = format!("{error:#}");
    assert!(!message.contains(SENTINEL), "{message}");
    assert!(message.contains("finishReason"), "{message}");
}

/// B1: an array that closed cleanly after a call-shaped prefix, with no
/// candidate ever reporting a terminal `finishReason`, is an INCOMPLETE turn.
/// The old rule (`finishReason` was optional usage metadata) returned those
/// calls as soon as the array closed.
#[tokio::test]
async fn closed_array_without_a_terminal_candidate_is_not_a_turn() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "test-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-token",
    );
    let body = array_stream(&[json!({"response": {
        "candidates": [{"content": {"role": "model", "parts": [{
            "thoughtSignature": "sig-call",
            "functionCall": {"name": "bash", "args": {"command": "echo hi"}, "id": "call_1"},
        }]}}],
        "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 2},
    }})]);
    let fake = spawn_fake(1, move |_, _| (200, body.clone())).await;
    let mut model = model_for(&fake, token_file, dir.path().join("cache/token.json"));
    let error = model
        .complete(
            &[Message::User {
                content: "run echo hi".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .expect_err("no terminal candidate means no completed turn");
    let message = format!("{error:#}");
    assert!(message.contains("finishReason"), "{message}");
}

/// B1, DECIDED RULE: a length-limited (`MAX_TOKENS`) answer is still
/// DISPLAYABLE as text — the caller sees the partial answer it is, with the
/// reason in the usage — but a length-limited CALL BATCH is refused: its
/// arguments can be cut mid-call, and a truncated call must never run.
#[tokio::test]
async fn length_limited_text_is_displayable_and_length_limited_calls_are_not() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "test-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-token",
    );
    let text = array_stream(&[json!({"response": {"candidates": [{
        "content": {"role": "model", "parts": [{"text": "the answer is 4"}]},
        "finishReason": "MAX_TOKENS",
    }],
    "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 5}}})]);
    let fake = spawn_fake(1, move |_, _| (200, text.clone())).await;
    let mut model = model_for(
        &fake,
        token_file.clone(),
        dir.path().join("cache/token.json"),
    );
    let (assistant, usage) = model
        .complete(
            &[Message::User {
                content: "compute".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .expect("length-limited text is a partial answer, not a failure");
    assert_eq!(assistant.content.as_deref(), Some("the answer is 4"));
    assert!(assistant.tool_calls.is_empty());
    assert_eq!(
        usage.expect("usage").finish_reason.as_deref(),
        Some("MAX_TOKENS"),
        "the truncation is reported, not hidden"
    );

    let calls = array_stream(&[json!({"response": {"candidates": [{
        "content": {"role": "model", "parts": [{
            "thoughtSignature": "sig-call",
            "functionCall": {"name": "bash", "args": {"command": "echo hi"}, "id": "call_1"},
        }]},
        "finishReason": "MAX_TOKENS",
    }]}})]);
    let fake = spawn_fake(1, move |_, _| (200, calls.clone())).await;
    let mut model = model_for(&fake, token_file, dir.path().join("cache/token.json"));
    let error = model
        .complete(
            &[Message::User {
                content: "run".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .expect_err("a truncated call batch must not run");
    let message = format!("{error:#}");
    assert!(message.contains("token limit"), "{message}");
    assert!(message.contains("not run"), "{message}");
}

/// B3: a fresh batch whose first call carries no signature can never be
/// replayed (400 INVALID_ARGUMENT), so it is refused WHILE assembling the turn
/// — before the caller runs a single call it could not send. The message is
/// the one the runner persists, so the provider-supplied id and tool name are
/// not in it.
#[tokio::test]
async fn unsigned_fresh_batch_is_refused_before_any_call_runs() {
    const CALL_ID: &str = "SENTINEL-CALL-ID-7d1f";
    const TOOL: &str = "SENTINEL-TOOL-NAME";
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "test-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-token",
    );
    let body = array_stream(&[json!({"response": {"candidates": [{
        "content": {"role": "model", "parts": [
            {"functionCall": {"name": TOOL, "args": {"command": "echo hi"}, "id": CALL_ID}},
            // An unsigned sibling is fine on its own; the FIRST call is what
            // must carry the batch signature.
            {"functionCall": {"name": TOOL, "args": {}, "id": "second"}},
        ]},
        "finishReason": "STOP",
    }]}})]);
    let fake = spawn_fake(1, move |_, _| (200, body.clone())).await;
    let mut model = model_for(&fake, token_file, dir.path().join("cache/token.json"));
    let error = model
        .complete(
            &[Message::User {
                content: "run".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .expect_err("an unreplayable batch is refused before it is returned");
    let message = format!("{error:#}");
    assert!(message.contains("thoughtSignature"), "{message}");
    assert!(message.contains("2 tool call(s)"), "{message}");
    assert!(message.contains("not run"), "{message}");
    assert!(!message.contains(CALL_ID), "{message}");
    assert!(!message.contains(TOOL), "{message}");
}

/// B3, DECIDED RULE: adjacency is preserved ACROSS elements. The wire may send
/// the call in one element and its signature-only part in the next; the
/// signature signs the immediately preceding call rather than being lost.
#[tokio::test]
async fn signature_only_part_in_the_next_element_signs_the_call_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "test-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-token",
    );
    let body = array_stream(&[
        json!({"response": {"candidates": [{"content": {"role": "model", "parts": [
            {"functionCall": {"name": "bash", "args": {"command": "echo a"}, "id": "call_a"}},
        ]}}]}}),
        json!({"response": {"candidates": [{"content": {"role": "model", "parts": [
            {"thoughtSignature": "SENTINEL-SIG-NEXT-ELEMENT", "text": ""},
        ]}, "finishReason": "STOP"}]}}),
    ]);
    let fake = spawn_fake(1, move |_, _| (200, body.clone())).await;
    let mut model = model_for(&fake, token_file, dir.path().join("cache/token.json"));
    let (assistant, _) = model
        .complete(
            &[Message::User {
                content: "run".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .expect("the batch is replayable once its signature arrives");
    assert_eq!(assistant.tool_calls.len(), 1);
    assert_eq!(
        assistant.tool_calls[0].provider_meta.as_deref(),
        Some("SENTINEL-SIG-NEXT-ELEMENT")
    );
    assert_eq!(assistant.content, None, "the empty text is not content");
}

/// H2: an existing permissive cache is REPLACED, not reopened — the refresh
/// leaves an owner-only file behind.
#[cfg(unix)]
#[tokio::test]
async fn refresh_replaces_a_permissive_cache_with_an_owner_only_file() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "expired-token",
        Utc::now() - ChronoDuration::minutes(5),
        "refresh-token",
    );
    let cache_file = dir.path().join("cache/antigravity-token.json");
    std::fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
    std::fs::write(&cache_file, b"{\"token\":{\"access_token\":\"stale\"}}").unwrap();
    std::fs::set_permissions(&cache_file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let body = array_stream(&[text_element("ok")]);
    let fake = spawn_fake(2, move |index, _| match index {
        0 => (
            200,
            json!({"access_token": "fresh-token", "expires_in": 3599}).to_string(),
        ),
        _ => (200, body.clone()),
    })
    .await;
    let mut model = model_for(&fake, token_file, cache_file.clone());
    model
        .complete(
            &[Message::User {
                content: "say ok".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .unwrap();
    let mode = std::fs::metadata(&cache_file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the replacement file is owner-only");
    assert_eq!(
        super::read_token(&cache_file)
            .unwrap()
            .unwrap()
            .token
            .access_token,
        "fresh-token"
    );
}

/// H2: a symlink at the cache path is not written through — it is replaced,
/// so a planted link cannot redirect the credential into another file.
#[cfg(unix)]
#[tokio::test]
async fn refresh_does_not_write_through_a_symlink_at_the_cache_path() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "expired-token",
        Utc::now() - ChronoDuration::minutes(5),
        "refresh-token",
    );
    let victim = dir.path().join("victim.txt");
    std::fs::write(&victim, b"untouched").unwrap();
    let cache_file = dir.path().join("cache/antigravity-token.json");
    std::fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&victim, &cache_file).unwrap();
    let body = array_stream(&[text_element("ok")]);
    let fake = spawn_fake(2, move |index, _| match index {
        0 => (
            200,
            json!({"access_token": "fresh-token", "expires_in": 3599}).to_string(),
        ),
        _ => (200, body.clone()),
    })
    .await;
    let mut model = model_for(&fake, token_file, cache_file.clone());
    model
        .complete(
            &[Message::User {
                content: "say ok".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read(&victim).unwrap(), b"untouched");
    let metadata = std::fs::symlink_metadata(&cache_file).unwrap();
    assert!(
        metadata.file_type().is_file(),
        "the destination is now a regular file, not a symlink"
    );
}

/// H3: a provider body never reaches the error string (it is persisted in
/// session history), only the status and an allowlisted code.
#[tokio::test]
async fn provider_error_bodies_are_never_surfaced() {
    const SENTINEL: &str = "SENTINEL-CREDENTIAL-1234";
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "test-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-token",
    );
    // HTTP error body with the sentinel in EVERY value field this code reads:
    // `code`, `type`, `details[0].reason`, plus `status` in the next case.
    // Allowlisting characters would let all of these through.
    let http_body = json!([{
        "error": {
            "code": SENTINEL,
            "type": SENTINEL,
            "status": "INVALID_ARGUMENT",
            "message": format!("token {SENTINEL} is not valid"),
            "details": [{"reason": SENTINEL, "metadata": {"token": SENTINEL}}],
        }
    }])
    .to_string();
    let first = spawn_fake(1, move |_, _| (400, http_body.clone())).await;
    let mut model = model_for(&first, token_file.clone(), dir.path().join("c/t.json"));
    let error = model
        .complete(
            &[Message::User {
                content: "hi".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .expect_err("HTTP 400 is a failure");
    let message = format!("{error:#}");
    assert!(!message.contains(SENTINEL), "{message}");
    assert!(message.contains("400"), "{message}");
    assert!(message.contains("INVALID_ARGUMENT"), "{message}");

    // No allowlisted value anywhere: the HTTP status alone, and in particular
    // nothing sentinel-shaped.
    let bare_body = json!([{
        "error": {"code": SENTINEL, "type": SENTINEL, "details": [{"reason": SENTINEL}]}
    }])
    .to_string();
    let bare = spawn_fake(1, move |_, _| (429, bare_body.clone())).await;
    let mut model = model_for(&bare, token_file.clone(), dir.path().join("c/t.json"));
    let error = model
        .complete(
            &[Message::User {
                content: "hi".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .expect_err("HTTP 429 is a failure");
    let message = format!("{error:#}");
    assert!(!message.contains(SENTINEL), "{message}");
    assert!(message.contains("429"), "{message}");
    assert!(!message.contains("SENTINEL"), "{message}");

    // In-stream error element: allowlisted `status`, sentinel everywhere else.
    let stream_body = array_stream(&[json!({"error": {
        "code": SENTINEL,
        "type": SENTINEL,
        "status": "INTERNAL",
        "message": format!("upstream rejected {SENTINEL}"),
        "details": [{"reason": SENTINEL}],
    }})]);
    let second = spawn_fake(1, move |_, _| (200, stream_body.clone())).await;
    let mut model = model_for(&second, token_file.clone(), dir.path().join("c/t.json"));
    let error = model
        .complete(
            &[Message::User {
                content: "hi".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .expect_err("an in-stream error is a failure");
    let message = format!("{error:#}");
    assert!(!message.contains(SENTINEL), "{message}");
    assert!(message.contains("INTERNAL"), "{message}");
    // A stream error with no allowlisted value reports no code at all.
    let bare_stream = spawn_fake(1, move |_, _| {
        (
            200,
            array_stream(&[json!({"error": {"status": SENTINEL, "type": SENTINEL}})]),
        )
    })
    .await;
    let mut model = model_for(&bare_stream, token_file, dir.path().join("c/t.json"));
    let error = model
        .complete(
            &[Message::User {
                content: "hi".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .expect_err("an in-stream error is a failure");
    let message = format!("{error:#}");
    assert!(!message.contains(SENTINEL), "{message}");
    assert_eq!(message.trim(), "Antigravity stream error", "{message}");
}

/// H3: a malformed credential file reports the path and the JSON position,
/// never a quote of the file.
#[test]
fn malformed_credential_file_reports_position_only() {
    const SENTINEL: &str = "SENTINEL-REFRESH-TOKEN-5678";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("antigravity-oauth-token");
    std::fs::write(
        &path,
        format!(r#"{{"token": {{"access_token": "{SENTINEL}" "refresh_token": 7}}}}"#),
    )
    .unwrap();
    let error = super::read_token(&path).expect_err("a malformed file is an error");
    let message = format!("{error:#}");
    assert!(!message.contains(SENTINEL), "{message}");
    assert!(message.contains("antigravity-oauth-token"), "{message}");
    assert!(message.contains("line 1"), "{message}");
    assert!(message.contains("column"), "{message}");
}

/// B2: a 3xx is a STATUS-ONLY failure. The refresh POST puts `client_secret`
/// and `refresh_token` in a replayable form body (and the generation POST the
/// whole conversation), so the client must not follow a `Location`: the
/// redirect target is never contacted, and neither the body nor the URL
/// reaches the message the runner persists.
#[tokio::test]
async fn redirects_are_not_followed_and_the_form_body_is_not_replayed() {
    const SENTINEL: &str = "SENTINEL-REFRESH-TOKEN-9c31";
    let dir = tempfile::tempdir().unwrap();
    // An expired login forces the refresh POST, whose form body carries the
    // credential a followed redirect would replay.
    let token_file = write_agy_token(
        dir.path(),
        "expired-token",
        Utc::now() - ChronoDuration::minutes(5),
        SENTINEL,
    );
    // Where a followed redirect WOULD land: nothing may ever connect here.
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_address = target.local_addr().unwrap();
    let location = format!("http://{target_address}/captured");
    let fake = spawn_responder(1, move |_, _| {
        let payload = json!({"error": "invalid_grant"}).to_string();
        format!(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        )
    })
    .await;
    let mut model = model_for(&fake, token_file, dir.path().join("cache/token.json"));
    let error = model
        .complete(
            &[Message::User {
                content: "hi".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .expect_err("a 3xx refresh is a failure");
    let message = format!("{error:#}");
    assert!(message.contains("307"), "{message}");
    assert!(!message.contains(SENTINEL), "{message}");
    assert!(
        !message.to_ascii_lowercase().contains("location"),
        "{message}"
    );
    assert!(
        !message.contains(&target_address.to_string()),
        "the redirect target is not even logged: {message}"
    );
    assert!(
        !message.contains("invalid_grant"),
        "a redirect body carries no verdict: {message}"
    );
    // The form body was sent exactly once, to the configured endpoint.
    assert_eq!(fake.requests().len(), 1);
    assert!(fake.raw_bodies()[0].contains("refresh_token="));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), target.accept())
            .await
            .is_err(),
        "the redirect target must never be contacted"
    );
}

/// H4: a cache bound to a PREVIOUS `agy` login is ignored, so a re-login
/// switches accounts instead of silently spending the old one.
#[tokio::test]
async fn cli_relogin_invalidates_the_bound_cache() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "new-account-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-new",
    );
    let cache_file = dir.path().join("cache/antigravity-token.json");
    // The old login's still-valid access token, bound to the OLD credential.
    write_cache(
        &cache_file,
        "old-account-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-old",
        "refresh-old",
    );
    let body = array_stream(&[text_element("ok")]);
    let fake = spawn_fake(1, move |_, _| (200, body.clone())).await;
    let mut model = model_for(&fake, token_file, cache_file);
    model
        .complete(
            &[Message::User {
                content: "hi".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .unwrap();
    let headers = fake.requests()[0].1.to_ascii_lowercase();
    assert!(
        headers.contains("authorization: bearer new-account-token"),
        "{headers}"
    );
    assert!(!headers.contains("old-account-token"), "{headers}");
}

/// H4: a cache whose fingerprint no longer matches is dropped even when its
/// access token still looks usable; the refresh then uses the CLI credential.
#[tokio::test]
async fn stale_fingerprint_forces_a_refresh_with_the_cli_credential() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "expired-token",
        Utc::now() - ChronoDuration::minutes(5),
        "refresh-new",
    );
    let cache_file = dir.path().join("cache/antigravity-token.json");
    write_cache(
        &cache_file,
        "cached-access-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-old",
        "refresh-old",
    );
    let body = array_stream(&[text_element("ok")]);
    let fake = spawn_fake(2, move |index, _| match index {
        0 => (
            200,
            json!({"access_token": "fresh-token", "expires_in": 3599}).to_string(),
        ),
        _ => (200, body.clone()),
    })
    .await;
    let mut model = model_for(&fake, token_file, cache_file.clone());
    model
        .complete(
            &[Message::User {
                content: "hi".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .unwrap();
    let form = fake.raw_bodies()[0].clone();
    assert!(form.contains("refresh_token=refresh-new"), "{form}");
    assert!(!form.contains("refresh-old"), "{form}");
    // The rebound cache now carries the CURRENT login's fingerprint.
    assert_eq!(
        super::read_token(&cache_file)
            .unwrap()
            .unwrap()
            .cli_fingerprint,
        Some(login_fingerprint("refresh-new"))
    );
}

/// H4: a corrupt CLI file is reported, not masked by a cache — e-agent cannot
/// verify which account the cache belongs to.
#[tokio::test]
async fn malformed_cli_file_is_not_masked_by_a_valid_cache() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("antigravity-oauth-token");
    std::fs::write(&token_file, b"{not json").unwrap();
    let cache_file = dir.path().join("cache/antigravity-token.json");
    write_cache(
        &cache_file,
        "cached-access-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-old",
        "refresh-old",
    );
    let fake = spawn_fake(1, move |_, _| (200, array_stream(&[text_element("ok")]))).await;
    let mut model = model_for(&fake, token_file, cache_file);
    let error = model
        .complete(
            &[Message::User {
                content: "hi".into(),
                images: Vec::new(),
            }],
            &[],
            None,
        )
        .await
        .expect_err("an unverifiable login is refused");
    let message = format!("{error:#}");
    assert!(message.contains("cannot decode"), "{message}");
    assert_eq!(fake.requests().len(), 0, "nothing was spent");
}

/// M1: a call the wire did not identify gets a unique id, and a batch that
/// repeats an id is rejected instead of silently routing two results to one
/// pending entry.
#[tokio::test]
async fn synthetic_ids_are_unique_and_duplicate_ids_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "test-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-token",
    );
    let idless = json!({"response": {"candidates": [{"content": {"role": "model", "parts": [
        {"thoughtSignature": "sig-a", "functionCall": {"name": "bash", "args": {"command": "a"}}},
    ]}, "finishReason": "STOP"}]}});
    let body = array_stream(std::slice::from_ref(&idless));
    let fake = spawn_fake(2, move |_, _| (200, body.clone())).await;
    let mut model = model_for(
        &fake,
        token_file.clone(),
        dir.path().join("cache/token.json"),
    );
    let user = Message::User {
        content: "run".into(),
        images: Vec::new(),
    };
    let (first, _) = model
        .complete(std::slice::from_ref(&user), &[], None)
        .await
        .unwrap();
    let (second, _) = model
        .complete(std::slice::from_ref(&user), &[], None)
        .await
        .unwrap();
    assert!(!first.tool_calls[0].id.is_empty());
    assert_ne!(
        first.tool_calls[0].id, second.tool_calls[0].id,
        "ids must not repeat across turns"
    );

    let duplicate = json!({"response": {"candidates": [{"content": {"role": "model", "parts": [
        {"thoughtSignature": "sig-a", "functionCall": {"name": "bash", "args": {}, "id": "call_1"}},
        {"thoughtSignature": "sig-b", "functionCall": {"name": "bash", "args": {}, "id": "call_1"}},
    ]}}]}});
    let dup_fake = spawn_fake(1, move |_, _| {
        (200, array_stream(std::slice::from_ref(&duplicate)))
    })
    .await;
    let mut dup_model = model_for(&dup_fake, token_file, dir.path().join("cache/token.json"));
    let error = dup_model
        .complete(std::slice::from_ref(&user), &[], None)
        .await
        .expect_err("a repeated id corrupts result routing");
    // The runner persists this text, so the provider-supplied id is replaced
    // by its ordinal and a short digest (B4). The digest still identifies
    // WHICH id repeated, without quoting it.
    let message = format!("{error:#}");
    assert!(message.contains("call 2 of this turn"), "{message}");
    assert!(
        message.contains(&short_digest("call_1")),
        "the digest names the repeated id: {message}"
    );
    assert!(!message.contains("call_1"), "{message}");
}

/// Pacing: consecutive turns are at least `min_interval` apart, measured on
/// the FAKE LISTENER's arrival times (not the client's clock), and the default
/// when the provider says nothing is the documented 1000 ms.
#[tokio::test]
async fn request_pacing_spaces_consecutive_turns() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "test-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-token",
    );
    let body = array_stream(&[text_element("ok")]);
    // A warm-up turn first, so the measured requests share the pooled
    // connection: an arrival stamp includes the client's own dispatch latency,
    // and a cold connect would cut into the FIRST measured gap for reasons the
    // pacer cannot observe.
    let fake = spawn_fake(3, move |_, _| (200, body.clone())).await;
    let mut model = model_for(&fake, token_file, dir.path().join("cache/token.json"));
    model.min_interval = PACING_TEST_INTERVAL;
    let user = Message::User {
        content: "say ok".into(),
        images: Vec::new(),
    };
    let started = Instant::now();
    for _ in 0..3 {
        model
            .complete(std::slice::from_ref(&user), &[], None)
            .await
            .unwrap();
    }
    // On the CLIENT's own clock nothing can shrink the wait: the two paced
    // requests slept a full interval each, so the sequence cannot be shorter
    // than 2 x interval (no dispatch or scheduler jitter is involved here).
    assert!(
        started.elapsed() >= PACING_TEST_INTERVAL * 2,
        "two paced requests waited at least two intervals: {:?}",
        started.elapsed()
    );
    let gaps = fake.gaps();
    assert_eq!(gaps.len(), 2, "three requests were made");
    for gap in &gaps {
        assert_paced(*gap, PACING_TEST_INTERVAL, &format!("{gaps:?}"));
    }

    // The default is the documented 1000 ms (60 RPM).
    let default =
        AntigravityModel::new("gemini-3.8-flash-tiered".into(), None, None, None, None).unwrap();
    assert_eq!(
        default.min_interval,
        std::time::Duration::from_millis(DEFAULT_MIN_INTERVAL_MS)
    );
    assert_eq!(DEFAULT_MIN_INTERVAL_MS, 1000);
}

/// Pacing: `min_interval_ms = 0` disables it explicitly — the second request
/// goes out at once, far below the 1000 ms default.
#[tokio::test]
async fn zero_min_interval_sends_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "test-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-token",
    );
    let body = array_stream(&[text_element("ok")]);
    let fake = spawn_fake(2, move |_, _| (200, body.clone())).await;
    let mut model = model_for(&fake, token_file, dir.path().join("cache/token.json"));
    model.min_interval = std::time::Duration::ZERO;
    let user = Message::User {
        content: "say ok".into(),
        images: Vec::new(),
    };
    for _ in 0..2 {
        model
            .complete(std::slice::from_ref(&user), &[], None)
            .await
            .unwrap();
    }
    let gaps = fake.gaps();
    assert_eq!(gaps.len(), 1);
    assert!(
        gaps[0] < PACING_TEST_INTERVAL,
        "unpaced requests are not delayed: {:?}",
        gaps[0]
    );
}

/// Pacing: a 401 -> refresh -> retry is three requests, and every one of them
/// waits its turn — the refresh is paced like any other request and the
/// retried generation does not overtake it.
#[tokio::test]
async fn refresh_and_retry_are_paced_too() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = write_agy_token(
        dir.path(),
        "stale-token",
        Utc::now() + ChronoDuration::hours(1),
        "refresh-token",
    );
    let body = array_stream(&[text_element("ok")]);
    // Connection warm-up (see `request_pacing_spaces_consecutive_turns`), then
    // the 401 -> refresh -> retry sequence on the same pooled connection.
    let fake = spawn_fake(4, move |index, path| match (index, path) {
        (0, _) => (200, body.clone()),
        (2, "/token") => (
            200,
            json!({"access_token": "fresh-token", "expires_in": 3599}).to_string(),
        ),
        (1, _) => (
            401,
            json!([{"error": {"code": 401, "status": "UNAUTHENTICATED"}}]).to_string(),
        ),
        _ => (200, body.clone()),
    })
    .await;
    let mut model = model_for(&fake, token_file, dir.path().join("cache/token.json"));
    model.min_interval = PACING_TEST_INTERVAL;
    let user = Message::User {
        content: "say ok".into(),
        images: Vec::new(),
    };
    model
        .complete(std::slice::from_ref(&user), &[], None)
        .await
        .expect("warm-up turn");
    let (assistant, _) = model
        .complete(std::slice::from_ref(&user), &[], None)
        .await
        .unwrap();
    assert_eq!(assistant.content.as_deref(), Some("ok"));
    let requests = fake.requests();
    assert_eq!(requests.len(), 4, "warm-up, 401, refresh, retry");
    assert!(requests[2].0.ends_with("/token"));
    // The LAST TWO gaps are the refresh and the retried generation: both are
    // requests, so both wait their turn.
    let gaps = fake.gaps();
    assert_eq!(gaps.len(), 3);
    for gap in &gaps {
        assert_paced(*gap, PACING_TEST_INTERVAL, &format!("{gaps:?}"));
    }
}

/// H5: the real builtin toolset — including `history`'s `oneOf`/`const` schema
/// and `bash` — rides the JSON-Schema field, not the Schema-proto
/// `parameters` field that rejects `const` (verified live: HTTP 400
/// INVALID_ARGUMENT for `parameters`, 200 + a real `history` call for
/// `parametersJsonSchema`).
#[test]
fn history_tool_schema_rides_the_wire() {
    let dir = tempfile::tempdir().unwrap();
    let model = AntigravityModel {
        client: reqwest::Client::new(),
        model: "gemini-3.8-flash-tiered".into(),
        reasoning_effort: None,
        stream_url: "http://127.0.0.1:1/stream".into(),
        token_endpoint: "http://127.0.0.1:1/token".into(),
        token_file: dir.path().join("agy.json"),
        cache_file: dir.path().join("cache.json"),
        client_id: None,
        client_secret: None,
        min_interval: std::time::Duration::ZERO,
        last_request: Default::default(),
    };
    let specs = builtin_specs(dir.path());
    assert!(specs.iter().any(|spec| spec.name == "history"));
    assert!(specs.iter().any(|spec| spec.name == "bash"));
    let body = model
        .request_body(
            &[Message::User {
                content: "read my history".into(),
                images: Vec::new(),
            }],
            &specs,
        )
        .unwrap();
    let declarations = body["request"]["tools"][0]["functionDeclarations"]
        .as_array()
        .unwrap();
    let history = declarations
        .iter()
        .find(|declaration| declaration["name"] == "history")
        .expect("history is declared");
    let expected = crate::agent::Tool::spec(&crate::tools::history::History).parameters;
    assert_eq!(
        history["parametersJsonSchema"], expected,
        "the real schema shape is what rides the wire"
    );
    assert!(
        history.get("parameters").is_none(),
        "the Schema-proto field rejects `const`: {history:?}"
    );
    assert!(
        history["parametersJsonSchema"]["oneOf"].is_array(),
        "the fixture really carries oneOf/const"
    );
    // `history`'s variants survive verbatim: the arguments the tool needs are
    // still declared, which is what a projection would have had to preserve.
    assert!(
        history["parametersJsonSchema"]["oneOf"][1]["properties"]["seq"].is_object(),
        "{history:?}"
    );
}

/// The real builtin tool specs the main session registers, in `history` /
/// `bash` form.
fn builtin_specs(root: &Path) -> Vec<ToolSpec> {
    let workspace = crate::workspace::Workspace::new(root).unwrap();
    let (tools, _background) = crate::tools::builtins(
        workspace,
        crate::config::SandboxPolicy::fixed(None),
        false,
        None,
    );
    tools.iter().map(|tool| tool.spec()).collect()
}

/// LIVE, ignored by default: the endpoint's actual verdicts — H5 (the builtin
/// toolset is accepted through `parametersJsonSchema` and `history` can still
/// be invoked) and H6 (a two-call round trip) — against the real OAuth login.
/// A refresh additionally needs the `agy` consumer client in
/// `E_AGENT_ANTIGRAVITY_CLIENT_ID` / `E_AGENT_ANTIGRAVITY_CLIENT_SECRET` (a
/// still-valid access token needs neither). Run with
/// `E_AGENT_ANTIGRAVITY_LIVE=1 cargo test --lib antigravity::tests::live_ -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "live network call against the Antigravity API; set E_AGENT_ANTIGRAVITY_LIVE=1"]
async fn live_builtin_toolset_and_parallel_round_trip() {
    if std::env::var("E_AGENT_ANTIGRAVITY_LIVE").is_err() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let specs = builtin_specs(dir.path());
    let mut model = AntigravityModel::new(
        "gemini-3.8-flash-tiered".into(),
        None,
        std::env::var("E_AGENT_ANTIGRAVITY_CLIENT_ID").ok(),
        std::env::var("E_AGENT_ANTIGRAVITY_CLIENT_SECRET").ok(),
        None,
    )
    .unwrap();

    // H5: the whole builtin toolset (history oneOf/const + bash + the rest) is
    // accepted, and `history` is invocable with the arguments its schema
    // declares.
    let (read_turn, _) = model
        .complete(
            &[Message::User {
                content:
                    "Call the history tool to READ transcript entry seq 7 of THIS session in one call. Then stop."
                        .into(),
                images: Vec::new(),
            }],
            &specs,
            None,
        )
        .await
        .expect("the builtin toolset (history oneOf/const + bash) is accepted");
    println!(
        "LIVE H5 calls={:?}",
        read_turn
            .tool_calls
            .iter()
            .map(|call| (call.name.clone(), call.arguments.clone()))
            .collect::<Vec<_>>()
    );
    let history_call = read_turn
        .tool_calls
        .iter()
        .find(|call| call.name == "history")
        .expect("the model can still invoke `history` through its real schema");
    let args: Value = serde_json::from_str(&history_call.arguments).unwrap();
    assert_eq!(args["action"], "read", "{args}");
    assert_eq!(args["seq"], 7, "{args}");

    // H6: a real two-call round trip.
    let user = Message::User {
        content: "Use the bash tool TWICE in this single turn: run `echo one` and `echo two` as two separate bash calls. Do not use any other tool.".into(),
        images: Vec::new(),
    };
    let (assistant, _) = model
        .complete(std::slice::from_ref(&user), &specs, None)
        .await
        .expect("a parallel-call turn is accepted");
    println!(
        "LIVE H6 calls={:?} signed={:?}",
        assistant
            .tool_calls
            .iter()
            .map(|call| (call.id.clone(), call.arguments.clone()))
            .collect::<Vec<_>>(),
        assistant
            .tool_calls
            .iter()
            .map(|call| call.provider_meta.is_some())
            .collect::<Vec<_>>()
    );
    assert!(
        assistant.tool_calls.len() >= 2,
        "the model should have asked for two calls"
    );
    let mut history = vec![user, Message::Assistant(assistant.clone())];
    for call in &assistant.tool_calls {
        history.push(Message::Tool {
            call_id: call.id.clone(),
            name: call.name.clone(),
            content: format!("ran {}", call.arguments),
            images: Vec::new(),
            is_error: false,
            synthetic: false,
        });
    }
    let (follow_up, _) = model
        .complete(&history, &specs, None)
        .await
        .expect("the two-call replay (each signature at its own position) is accepted");
    println!(
        "LIVE H6 parallel verdict: two-call round trip accepted; follow-up content={:?}",
        follow_up.content
    );
}
