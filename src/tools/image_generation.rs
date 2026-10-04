//! The one concrete `generate_image` tool: a text prompt becomes one PNG
//! file below the workspace root through the ChatGPT Codex image endpoint
//! (direct HTTP, ordinary JSON). Deliberately concrete: no SDK, no CLI, no
//! account store, no API-key fallback, no provider trait, and no automatic
//! retry after an ambiguous network failure.

use std::path::{Component, Path};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use serde_json::{Value, json};

use super::spec;
use crate::agent::{Tool, ToolOutput, ToolSpec};
use crate::workspace::Workspace;

/// The public Codex image generations endpoint (`POST`).
const IMAGE_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/images/generations";
/// Fixed wire values for every request; there are no per-call knobs beyond
/// the prompt and the optional transparent background.
const IMAGE_MODEL: &str = "gpt-image-2";
const IMAGE_QUALITY: &str = "auto";
const IMAGE_SIZE: &str = "auto";
const ORIGINATOR: &str = "codex_cli_rs";
const IMAGE_TURN_ID_HEADER: &str = "x-codex-image-turn-id";
const IMAGEGEN_REQUEST_ID_HEADER: &str = "x-codex-imagegen-request-id";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
/// Explicit bounds for the prompt, the raw response body, and the decoded
/// image bytes. Neither body is ever read unbounded.
const MAX_PROMPT_BYTES: usize = 32_000;
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// Resolves the shared ChatGPT credentials this tool sends with. The main
/// session factory supplies its lazy reloadable-cache resolver (see
/// `session_factory::lazy_chatgpt_auth`), so a login that arrived after
/// startup is picked up on first use without a restart; tests supply a
/// fixture handle.
pub(crate) type AuthResolver =
    Arc<dyn Fn() -> anyhow::Result<crate::codex_auth::CodexAuth> + Send + Sync>;

/// The registered tool: `generate_image` on the main session only (the
/// session factory decides registration; read-only sessions skip it).
pub(crate) fn generate_image_tool(
    workspace: Workspace,
    auth: AuthResolver,
) -> Result<Box<dyn Tool>, String> {
    Ok(Box::new(GenerateImage {
        workspace,
        auth,
        endpoint: IMAGE_ENDPOINT.to_owned(),
        client: image_client(REQUEST_TIMEOUT)?,
        max_response_bytes: MAX_RESPONSE_BYTES,
        max_image_bytes: MAX_IMAGE_BYTES,
    }))
}

fn image_client(timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .map_err(|error| format!("cannot create image generation HTTP client: {error}"))
}

struct GenerateImage {
    workspace: Workspace,
    auth: AuthResolver,
    endpoint: String,
    client: reqwest::Client,
    max_response_bytes: usize,
    max_image_bytes: usize,
}

impl GenerateImage {
    #[cfg(test)]
    fn for_test(
        workspace: Workspace,
        auth: AuthResolver,
        endpoint: String,
        timeout: Duration,
        max_response_bytes: usize,
        max_image_bytes: usize,
    ) -> Self {
        Self {
            workspace,
            auth,
            endpoint,
            client: image_client(timeout).expect("test image client"),
            max_response_bytes,
            max_image_bytes,
        }
    }

    /// One bounded `POST`; returns the decoded image bytes and the optional
    /// provider request id. A 401 is a normal tool error calling for
    /// `e-agent login` — there is no charged retry loop.
    async fn request_image(
        &self,
        prompt: &str,
        transparent: bool,
        access_token: &str,
        account_id: &str,
    ) -> Result<(Vec<u8>, Option<String>), String> {
        // No idempotency key: the provider makes no idempotency promise, and
        // a retry after an ambiguous failure could generate twice.
        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(access_token)
            .header("ChatGPT-Account-ID", account_id)
            .header("originator", ORIGINATOR)
            .header(IMAGE_TURN_ID_HEADER, uuid_v4())
            .json(&json!({
                "prompt": prompt,
                "background": if transparent { "transparent" } else { "opaque" },
                "model": IMAGE_MODEL,
                "quality": IMAGE_QUALITY,
                "size": IMAGE_SIZE,
            }))
            .send()
            .await
            .map_err(|error| format!("image generation request failed: {error}"))?;
        let status = response.status();
        let provider_request_id = response
            .headers()
            .get(IMAGEGEN_REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        // A failed status is reported by status (+ provider request id) only
        // and its body is never read, echoed, or retried.
        if !status.is_success() {
            return Err(provider_error(status, provider_request_id.as_deref()));
        }
        let body = read_bounded(response, self.max_response_bytes).await?;
        decode_single_image(&body, self.max_image_bytes).map(|bytes| (bytes, provider_request_id))
    }
}

#[async_trait]
impl Tool for GenerateImage {
    fn spec(&self) -> ToolSpec {
        spec(
            "generate_image",
            "Generate one image from a text prompt with the ChatGPT Codex image backend and save it as a new PNG file below the workspace. Requires an existing ChatGPT login (`e-agent login`). `path` must be a workspace-relative `.png` path that does not already exist; describe content, style, composition, and any text in `prompt`.",
            json!({
                "prompt": {"type": "string", "description": "the image prompt; non-empty, at most 32000 bytes"},
                "path": {"type": "string", "description": "workspace-relative `.png` output path that must not already exist"},
                "transparent_background": {"type": "boolean", "description": "use a transparent background (default false = opaque)"}
            }),
            &["prompt", "path"],
        )
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, String> {
        let (prompt, path, transparent) = parse_arguments(&arguments)?;
        // All path/policy checks run before any provider request, so an
        // invalid, occupied, or unwritable target never spends anything. The
        // output must stay inside the workspace itself: a configured
        // writable external root does not widen this tool. The final
        // exclusive create stays the authority against a race.
        if !self.workspace.targets_workspace(&path)? {
            return Err(format!(
                "`{path}` must be inside the workspace; configured external roots are not writable by generate_image"
            ));
        }
        if self.workspace.exists(&path)? {
            return Err(format!(
                "`{path}` already exists; generate_image never overwrites (no request was sent)"
            ));
        }
        // Policy (read-only workspace / protected policy file) is checked
        // before the request too: a write that could never publish must not
        // spend a generation.
        self.workspace.check_writable(&path)?;
        let auth = (self.auth)().map_err(|error| format!("{error:#}"))?;
        let (access_token, account_id) = auth
            .access_token_and_account()
            .await
            .map_err(|error| format!("{error:#}"))?;
        let (bytes, provider_request_id) = self
            .request_image(&prompt, transparent, &access_token, &account_id)
            .await?;
        // The whole file is structural-checked before it becomes visible.
        validate_png(&bytes)?;
        self.workspace.write_new(&path, &bytes)?;
        let mut output = format!("generated image saved to {path} ({} bytes)", bytes.len());
        if let Some(request_id) = provider_request_id {
            output.push_str(&format!("; request id {request_id}"));
        }
        Ok(ToolOutput::text(output))
    }
}

/// Strict argument parsing: unknown keys are rejected, and the path is
/// validated lexically (traversal, absolute and Windows-like forms) before
/// any workspace or network operation.
fn parse_arguments(arguments: &Value) -> Result<(String, String, bool), String> {
    let object = arguments
        .as_object()
        .ok_or("tool arguments must be a JSON object")?;
    for key in object.keys() {
        if !matches!(key.as_str(), "prompt" | "path" | "transparent_background") {
            return Err(format!("unknown argument `{key}`"));
        }
    }
    let prompt = object
        .get("prompt")
        .and_then(Value::as_str)
        .ok_or("`prompt` must be a string")?
        .to_owned();
    if prompt.trim().is_empty() {
        return Err("`prompt` must not be empty".into());
    }
    if prompt.len() > MAX_PROMPT_BYTES {
        return Err(format!("`prompt` exceeds {MAX_PROMPT_BYTES} bytes"));
    }
    let path = object
        .get("path")
        .and_then(Value::as_str)
        .ok_or("`path` must be a string")?
        .to_owned();
    validate_output_path(&path)?;
    let transparent = match object.get("transparent_background") {
        None => false,
        Some(value) => value
            .as_bool()
            .ok_or("`transparent_background` must be a boolean")?,
    };
    Ok((prompt, path, transparent))
}

fn validate_output_path(path: &str) -> Result<(), String> {
    let invalid = || {
        "`path` must be a non-empty workspace-relative `.png` path with only normal components"
            .to_owned()
    };
    if path.is_empty()
        || path.contains('\\')
        || path.contains(':')
        || !path.ends_with(".png")
        || Path::new(path).is_absolute()
        || Path::new(path)
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(invalid());
    }
    Ok(())
}

/// UUID-shaped per-operation turn id (version 4, variant 10xx) from 16
/// random bytes, matching the shape the Codex CLI sends without pulling in
/// a UUID crate.
fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Read the complete response body under an explicit byte cap, without a
/// detached reader task: dropping the tool future drops this future and
/// stops the HTTP request itself.
async fn read_bounded(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("image generation response body failed: {error}"))?
    {
        if body.len() + chunk.len() > limit {
            return Err(format!("image generation response exceeds {limit} bytes"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[derive(Deserialize)]
struct ImagesResponse {
    #[serde(default)]
    data: Vec<ImageDatum>,
}

#[derive(Deserialize)]
struct ImageDatum {
    #[serde(default)]
    b64_json: Option<String>,
}

/// Decode the response's single base64 image under the decoded-size cap.
/// Empty, multiple, and non-`b64_json` entries are rejected.
fn decode_single_image(body: &[u8], max_image_bytes: usize) -> Result<Vec<u8>, String> {
    let response: ImagesResponse = serde_json::from_slice(body)
        .map_err(|error| format!("cannot decode image generation response: {error}"))?;
    let [image] = response.data.as_slice() else {
        return Err(format!(
            "image generation response carried {} images; exactly one is required",
            response.data.len()
        ));
    };
    let encoded = image
        .b64_json
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or("image generation response carried no `b64_json` image data")?;
    let too_large = || format!("generated image exceeds {max_image_bytes} bytes");
    // Bound the encoded length before allocating the decoded buffer.
    if encoded.len() > max_image_bytes.div_ceil(3) * 4 + 4 {
        return Err(too_large());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| "image generation response carried invalid base64 image data".to_owned())?;
    if bytes.len() > max_image_bytes {
        return Err(too_large());
    }
    Ok(bytes)
}

/// Bounded, non-reflecting error text: the HTTP status plus the optional
/// provider request id (and the login hint for 401). The response body, the
/// request payload, and credentials are never echoed.
fn provider_error(status: reqwest::StatusCode, request_id: Option<&str>) -> String {
    let mut text = format!("image generation request failed with HTTP {status}");
    if let Some(request_id) = request_id {
        text.push_str(&format!(" (request id {request_id})"));
    }
    if status == reqwest::StatusCode::UNAUTHORIZED {
        text.push_str("; run `e-agent login` and try again");
    }
    text
}

/// Structural PNG sanity from the signature and the IHDR header only — no
/// decode and no visual claim: signature, a first chunk of exactly the
/// 13-byte IHDR, and non-zero dimensions.
fn validate_png(bytes: &[u8]) -> Result<(), String> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    if bytes.len() < 33 || bytes[..8] != SIGNATURE {
        return Err("generated file is not a PNG image".into());
    }
    let length = u32::from_be_bytes(bytes[8..12].try_into().unwrap());
    if length != 13 || &bytes[12..16] != b"IHDR" {
        return Err("generated PNG is missing a well-formed IHDR header".into());
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    if width == 0 || height == 0 {
        return Err("generated PNG has zero dimensions".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_auth::CodexAuth;
    use crate::tools::BackgroundTasks;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const CONNECT_WAIT: Duration = Duration::from_millis(250);

    /// Fixture workspace plus a fixture ChatGPT handle whose path never
    /// exists: `access_token_and_account` reads no real credential store and
    /// needs no token refresh (`last_refresh` is "now", the token is not a
    /// JWT).
    struct Fixture {
        temp: tempfile::TempDir,
        workspace: Workspace,
        auth: AuthResolver,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let workspace = Workspace::new(temp.path()).unwrap();
            let auth = CodexAuth::test_auth(temp.path().join("auth.json"));
            Self {
                temp,
                workspace,
                auth: Arc::new(move || Ok(auth.clone())),
            }
        }

        fn tool(
            &self,
            endpoint: &str,
            max_response_bytes: usize,
            max_image_bytes: usize,
        ) -> GenerateImage {
            GenerateImage::for_test(
                self.workspace.clone(),
                self.auth.clone(),
                endpoint.to_owned(),
                Duration::from_secs(2),
                max_response_bytes,
                max_image_bytes,
            )
        }
    }

    /// Minimal structurally valid PNG payload (signature + IHDR); good
    /// enough for the signature/IHDR validation, never decoded here.
    fn png_bytes() -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&2u32.to_be_bytes());
        bytes.extend_from_slice(&3u32.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes
    }

    fn base64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn image_response(bytes: &[u8]) -> Vec<u8> {
        let body = json!({
            "created": 1,
            "data": [{"b64_json": base64(bytes)}],
        })
        .to_string();
        http_response(
            "200 OK",
            &[(IMAGEGEN_REQUEST_ID_HEADER, "test-image-request")],
            body.as_bytes(),
        )
    }

    fn http_response(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
        let mut response = format!("HTTP/1.1 {status}\r\n").into_bytes();
        for (name, value) in headers {
            response.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        response.extend_from_slice(
            format!(
                "Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        );
        response.extend_from_slice(body);
        response
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        let header_end = loop {
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0, "client closed before completing its request");
            request.extend_from_slice(&buffer[..count]);
            if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = std::str::from_utf8(&request[..header_end]).unwrap();
        let content_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length: "))
            .unwrap()
            .parse::<usize>()
            .unwrap();
        while request.len() < header_end + content_length {
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0, "client closed before sending its request body");
            request.extend_from_slice(&buffer[..count]);
        }
        request
    }

    /// One mock server per test action: it answers every request with the
    /// same response, records the request bytes, and stops after
    /// `CONNECT_WAIT` without a new connection (so a forbidden retry would
    /// show up as a second recorded request).
    async fn serve(response: Vec<u8>) -> (String, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "http://{}/backend-api/codex/images/generations",
            listener.local_addr().unwrap()
        );
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            while let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(CONNECT_WAIT, listener.accept()).await
            {
                requests.push(read_request(&mut socket).await);
                let _ = socket.write_all(&response).await;
                let _ = socket.shutdown().await;
            }
            requests
        });
        (endpoint, task)
    }

    async fn run_case(
        fixture: &Fixture,
        response: Vec<u8>,
        arguments: Value,
    ) -> (Result<ToolOutput, String>, Vec<Vec<u8>>) {
        let (endpoint, server) = serve(response).await;
        let tool = fixture.tool(&endpoint, MAX_RESPONSE_BYTES, MAX_IMAGE_BYTES);
        let result = tool.execute(arguments).await;
        (result, server.await.unwrap())
    }

    fn header_value(request: &[u8], name: &str) -> String {
        let text = String::from_utf8_lossy(request);
        let header_end = text.find("\r\n\r\n").unwrap();
        text[..header_end]
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}: ")))
            .unwrap_or_else(|| panic!("missing {name} header"))
            .to_owned()
    }

    fn request_json(request: &[u8]) -> Value {
        let text = String::from_utf8_lossy(request);
        let body_start = text.find("\r\n\r\n").unwrap() + 4;
        serde_json::from_str(&text[body_start..]).unwrap()
    }

    fn assert_uuid_shaped(value: &str) {
        let parts: Vec<&str> = value.split('-').collect();
        assert_eq!(parts.len(), 5, "UUID shape: {value}");
        let lengths: Vec<usize> = parts.iter().map(|part| part.len()).collect();
        assert_eq!(lengths, [8, 4, 4, 4, 12], "UUID shape: {value}");
        assert!(parts[2].starts_with('4'), "version 4: {value}");
        assert!(
            matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'),
            "variant: {value}"
        );
    }

    #[tokio::test]
    async fn request_headers_payload_and_saved_bytes_match_the_contract() {
        let fixture = Fixture::new();
        let png = png_bytes();
        let (endpoint, server) = serve(image_response(&png)).await;
        let tool = fixture.tool(&endpoint, MAX_RESPONSE_BYTES, MAX_IMAGE_BYTES);
        let first = tool
            .execute(json!({"prompt": "a cat", "path": "art/cat.png"}))
            .await
            .unwrap();
        assert!(first.content.contains("art/cat.png"), "{}", first.content);
        assert!(first.content.contains("test-image-request"));
        // Only the relative path is reported, never the workspace path.
        assert!(
            !first
                .content
                .contains(fixture.temp.path().to_str().unwrap())
        );
        assert_eq!(
            std::fs::read(fixture.temp.path().join("art/cat.png")).unwrap(),
            png
        );
        let second = tool
            .execute(json!({
                "prompt": "a dog",
                "path": "dog.png",
                "transparent_background": true,
            }))
            .await
            .unwrap();
        assert!(second.content.contains("dog.png"));

        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            String::from_utf8_lossy(&requests[0])
                .starts_with("POST /backend-api/codex/images/generations HTTP/1.1")
        );
        assert_eq!(header_value(&requests[0], "authorization"), "Bearer access");
        assert_eq!(header_value(&requests[0], "chatgpt-account-id"), "account");
        assert_eq!(header_value(&requests[0], "originator"), "codex_cli_rs");
        let first_turn = header_value(&requests[0], "x-codex-image-turn-id");
        assert_uuid_shaped(&first_turn);
        let second_turn = header_value(&requests[1], "x-codex-image-turn-id");
        assert_uuid_shaped(&second_turn);
        assert_ne!(first_turn, second_turn, "turn id is per operation");

        let payload = request_json(&requests[0]);
        assert_eq!(payload["prompt"], "a cat");
        assert_eq!(payload["background"], "opaque");
        assert_eq!(payload["model"], "gpt-image-2");
        assert_eq!(payload["quality"], "auto");
        assert_eq!(payload["size"], "auto");
        assert_eq!(
            payload.as_object().unwrap().len(),
            5,
            "no `n` or other fields: {payload}"
        );
        assert_eq!(request_json(&requests[1])["background"], "transparent");
        assert!(
            !String::from_utf8_lossy(&requests[0]).contains(&base64(&png)),
            "base64 must not be echoed into the request"
        );
    }

    #[tokio::test]
    async fn response_validation_requires_exactly_one_valid_base64_png() {
        let fixture = Fixture::new();
        let cases: Vec<(Value, &str)> = vec![
            (json!({"created": 1, "data": []}), "exactly one"),
            (
                json!({"created": 1, "data": [{"b64_json": "aGk="}, {"b64_json": "aGk="}]}),
                "exactly one",
            ),
            (
                json!({"created": 1, "data": [{"url": "http://x"}]}),
                "`b64_json`",
            ),
            (
                json!({"created": 1, "data": [{"b64_json": "!!!"}]}),
                "invalid base64",
            ),
            (
                json!({"created": 1, "data": [{"b64_json": "aGk="}]}),
                "not a PNG",
            ),
            (
                json!({"created": 1, "data": [{"b64_json": "aGVsbG8="}]}),
                "not a PNG",
            ),
        ];
        for (index, (body, expected)) in cases.into_iter().enumerate() {
            let path = format!("case{index}.png");
            let response = http_response("200 OK", &[], body.to_string().as_bytes());
            let (result, requests) =
                run_case(&fixture, response, json!({"prompt": "x", "path": path})).await;
            let error = result.unwrap_err();
            assert!(error.contains(expected), "case {index}: {error}");
            assert_eq!(requests.len(), 1, "case {index}");
            assert!(!fixture.temp.path().join(&path).exists(), "case {index}");
        }
    }

    #[tokio::test]
    async fn oversized_response_and_image_bytes_are_rejected_with_explicit_limits() {
        let fixture = Fixture::new();
        // The raw body over the response cap is rejected while streaming.
        let (endpoint, server) = serve(http_response("200 OK", &[], &vec![b'x'; 4096])).await;
        let tool = fixture.tool(&endpoint, 1024, 512);
        let error = tool
            .execute(json!({"prompt": "x", "path": "big.png"}))
            .await
            .unwrap_err();
        assert!(error.contains("exceeds 1024 bytes"), "{error}");
        assert_eq!(server.await.unwrap().len(), 1);
        assert!(!fixture.temp.path().join("big.png").exists());

        // Valid JSON whose decoded image bytes exceed the decoded cap.
        let (endpoint, server) = serve(image_response(&vec![0u8; 600])).await;
        let tool = fixture.tool(&endpoint, 4096, 512);
        let error = tool
            .execute(json!({"prompt": "x", "path": "big.png"}))
            .await
            .unwrap_err();
        assert!(error.contains("exceeds 512 bytes"), "{error}");
        assert!(!fixture.temp.path().join("big.png").exists());
        assert_eq!(server.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn http_errors_are_bounded_not_reflected_and_never_retried() {
        let fixture = Fixture::new();
        let secret_body = json!({
            "error": {"message": "request denied", "code": "denied"},
            "access_token": "ACCESS-SECRET",
            "prompt": "PROMPT-SECRET",
            "b64_json": "B64-SECRET",
        })
        .to_string();
        for status in [
            "401 Unauthorized",
            "403 Forbidden",
            "429 Too Many Requests",
            "500 Internal Server Error",
        ] {
            let response = http_response(status, &[], secret_body.as_bytes());
            let (result, requests) = run_case(
                &fixture,
                response,
                json!({"prompt": "PROMPT-SECRET", "path": "e.png"}),
            )
            .await;
            let error = result.unwrap_err();
            let code = status.split(' ').next().unwrap();
            assert!(error.contains(code), "{status}: {error}");
            // The provider body — even a well-formed JSON error — is never
            // echoed: status, optional request id, and the login hint only.
            for secret in ["denied", "ACCESS-SECRET", "PROMPT-SECRET", "B64-SECRET"] {
                assert!(!error.contains(secret), "{status}: {error}");
            }
            if code == "401" {
                assert!(error.contains("e-agent login"), "{status}: {error}");
            }
            assert_eq!(requests.len(), 1, "no retry after {status}");
        }
        // A raw non-JSON body is never echoed either; the optional provider
        // request id is the only extra detail.
        let response = http_response(
            "500 Internal Server Error",
            &[(IMAGEGEN_REQUEST_ID_HEADER, "req-123")],
            b"sk-live-SECRET raw body",
        );
        let (result, requests) =
            run_case(&fixture, response, json!({"prompt": "x", "path": "e.png"})).await;
        let error = result.unwrap_err();
        assert!(error.contains("HTTP 500"), "{error}");
        assert!(error.contains("req-123"), "{error}");
        assert!(!error.contains("sk-live-SECRET"), "{error}");
        assert_eq!(requests.len(), 1);
        // A failed status does not read the body at all: even a body larger
        // than the response cap reports the status, not a size error.
        let response = http_response("500 Internal Server Error", &[], &vec![b'x'; 4096]);
        let (endpoint, server) = serve(response).await;
        let tool = fixture.tool(&endpoint, 1024, 512);
        let error = tool
            .execute(json!({"prompt": "x", "path": "e.png"}))
            .await
            .unwrap_err();
        assert!(error.contains("HTTP 500"), "{error}");
        assert!(!error.contains("exceeds"), "{error}");
        assert_eq!(server.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn redirects_are_not_followed() {
        let fixture = Fixture::new();
        let (target_endpoint, target_server) = serve(image_response(&png_bytes())).await;
        let response = http_response("302 Found", &[("Location", &target_endpoint)], b"");
        let (result, requests) =
            run_case(&fixture, response, json!({"prompt": "x", "path": "r.png"})).await;
        let error = result.unwrap_err();
        assert!(error.contains("302"), "{error}");
        assert_eq!(requests.len(), 1);
        assert!(target_server.await.unwrap().is_empty(), "redirect followed");
        assert!(!fixture.temp.path().join("r.png").exists());
    }

    #[tokio::test]
    async fn missing_login_is_actionable_and_sends_nothing() {
        let fixture = Fixture::new();
        let (endpoint, server) = serve(image_response(&png_bytes())).await;
        let auth: AuthResolver = Arc::new(|| {
            Err(anyhow::anyhow!(
                "ChatGPT auth is not initialized (cannot read auth file); run `e-agent login`"
            ))
        });
        let tool = GenerateImage::for_test(
            fixture.workspace.clone(),
            auth,
            endpoint,
            Duration::from_secs(2),
            MAX_RESPONSE_BYTES,
            MAX_IMAGE_BYTES,
        );
        let error = tool
            .execute(json!({"prompt": "x", "path": "n.png"}))
            .await
            .unwrap_err();
        assert!(error.contains("e-agent login"), "{error}");
        assert!(server.await.unwrap().is_empty());
        assert!(!fixture.temp.path().join("n.png").exists());
    }

    #[tokio::test]
    async fn invalid_arguments_and_paths_are_rejected_before_any_request() {
        let fixture = Fixture::new();
        let (endpoint, server) = serve(image_response(&png_bytes())).await;
        let tool = fixture.tool(&endpoint, MAX_RESPONSE_BYTES, MAX_IMAGE_BYTES);
        let long_prompt = "p".repeat(MAX_PROMPT_BYTES + 1);
        let cases: Vec<(Value, &str)> = vec![
            (json!([1]), "JSON object"),
            (json!({}), "`prompt`"),
            (json!({"prompt": "", "path": "a.png"}), "must not be empty"),
            (
                json!({"prompt": "   ", "path": "a.png"}),
                "must not be empty",
            ),
            (
                json!({"prompt": long_prompt, "path": "a.png"}),
                "exceeds 32000 bytes",
            ),
            (
                json!({"prompt": "x", "path": "a.png", "n": 1}),
                "unknown argument",
            ),
            (
                json!({"prompt": "x", "path": "a.png", "transparent_background": "yes"}),
                "must be a boolean",
            ),
            (json!({"prompt": "x", "path": ""}), "workspace-relative"),
            (json!({"prompt": "x", "path": "a"}), "workspace-relative"),
            (
                json!({"prompt": "x", "path": "a.txt"}),
                "workspace-relative",
            ),
            (
                json!({"prompt": "x", "path": "../a.png"}),
                "workspace-relative",
            ),
            (
                json!({"prompt": "x", "path": "dir/../../a.png"}),
                "workspace-relative",
            ),
            (
                json!({"prompt": "x", "path": "./a.png"}),
                "workspace-relative",
            ),
            (
                json!({"prompt": "x", "path": "/tmp/a.png"}),
                "workspace-relative",
            ),
            (
                json!({"prompt": "x", "path": "..\\a.png"}),
                "workspace-relative",
            ),
            (
                json!({"prompt": "x", "path": "C:\\tmp\\a.png"}),
                "workspace-relative",
            ),
        ];
        for (index, (arguments, expected)) in cases.into_iter().enumerate() {
            let error = tool.execute(arguments).await.unwrap_err();
            assert!(error.contains(expected), "case {index}: {error}");
        }
        assert!(
            server.await.unwrap().is_empty(),
            "invalid arguments must not reach the provider"
        );
        assert!(!fixture.temp.path().join("a.png").exists());
    }

    #[tokio::test]
    async fn existing_output_is_rejected_before_any_request() {
        let fixture = Fixture::new();
        let taken = fixture.temp.path().join("taken.png");
        std::fs::write(&taken, b"user bytes").unwrap();
        let (endpoint, server) = serve(image_response(&png_bytes())).await;
        let tool = fixture.tool(&endpoint, MAX_RESPONSE_BYTES, MAX_IMAGE_BYTES);
        let error = tool
            .execute(json!({"prompt": "x", "path": "taken.png"}))
            .await
            .unwrap_err();
        assert!(error.contains("already exists"), "{error}");
        assert_eq!(std::fs::read(&taken).unwrap(), b"user bytes");
        assert!(server.await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn external_root_output_is_rejected_before_any_request() {
        let temp = tempfile::tempdir().unwrap();
        let child = temp.path().join("child");
        std::fs::create_dir(&child).unwrap();
        // A writable external entry that is (lexically) inside the
        // workspace: a relative path that resolves to it must still be
        // refused, because generate_image only writes below the workspace.
        let policy = crate::config::Sandbox {
            writable_paths: vec![child.to_str().unwrap().to_owned()],
            ..Default::default()
        };
        let workspace = Workspace::new(temp.path())
            .unwrap()
            .with_external_roots(&policy)
            .unwrap();
        let auth = CodexAuth::test_auth(temp.path().join("auth.json"));
        let auth: AuthResolver = Arc::new(move || Ok(auth.clone()));
        let (endpoint, server) = serve(image_response(&png_bytes())).await;
        let tool = GenerateImage::for_test(
            workspace,
            auth,
            endpoint,
            Duration::from_secs(2),
            MAX_RESPONSE_BYTES,
            MAX_IMAGE_BYTES,
        );
        let error = tool
            .execute(json!({"prompt": "x", "path": "child/out.png"}))
            .await
            .unwrap_err();
        assert!(error.contains("inside the workspace"), "{error}");
        assert!(server.await.unwrap().is_empty());
        assert!(!child.join("out.png").exists());
    }

    #[tokio::test]
    async fn read_only_workspace_is_refused_before_any_request() {
        let temp = tempfile::tempdir().unwrap();
        let policy = crate::config::Sandbox {
            enabled: true,
            workspace_writable: false,
            ..Default::default()
        };
        let workspace = Workspace::new(temp.path())
            .unwrap()
            .with_external_roots(&policy)
            .unwrap();
        let (endpoint, server) = serve(image_response(&png_bytes())).await;
        let tool = GenerateImage::for_test(
            workspace,
            Arc::new(|| unreachable!("auth must not be resolved for an unwritable target")),
            endpoint,
            Duration::from_secs(2),
            MAX_RESPONSE_BYTES,
            MAX_IMAGE_BYTES,
        );
        let error = tool
            .execute(json!({"prompt": "x", "path": "ro.png"}))
            .await
            .unwrap_err();
        assert!(error.contains("read-only"), "{error}");
        assert!(server.await.unwrap().is_empty());
        assert!(!temp.path().join("ro.png").exists());
    }

    #[tokio::test]
    async fn concurrent_creates_publish_exactly_one_file() {
        let fixture = Fixture::new();
        let png = png_bytes();
        let (endpoint, server) = serve(image_response(&png)).await;
        let tool = fixture.tool(&endpoint, MAX_RESPONSE_BYTES, MAX_IMAGE_BYTES);
        let args = || json!({"prompt": "race", "path": "race.png"});
        let (first, second) = tokio::join!(tool.execute(args()), tool.execute(args()));
        let results = [first, second];
        assert_eq!(
            results.iter().filter(|result| result.is_ok()).count(),
            1,
            "{results:?}"
        );
        let error = results
            .iter()
            .find(|result| result.is_err())
            .and_then(|result| result.as_ref().err())
            .unwrap();
        assert!(error.contains("already exists"), "{error}");
        assert_eq!(
            std::fs::read(fixture.temp.path().join("race.png")).unwrap(),
            png,
            "the winner's bytes stay untouched"
        );
        assert_eq!(
            server.await.unwrap().len(),
            2,
            "both calls raced past the early occupied-path check"
        );
    }

    #[tokio::test]
    async fn dropping_the_tool_future_after_partial_body_publishes_nothing() {
        let fixture = Fixture::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "http://{}/backend-api/codex/images/generations",
            listener.local_addr().unwrap()
        );
        let (complete_tx, complete_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut socket).await;
            let full = image_response(&png_bytes());
            let split = full
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .unwrap()
                + 4;
            let (head, body) = full.split_at(split);
            // Headers plus a partial body, then hold: the tool future is
            // still waiting for the rest when the test drops it.
            let _ = socket.write_all(head).await;
            let _ = socket.write_all(&body[..body.len() / 2]).await;
            let _ = complete_rx.await;
            // Even a later complete body must publish nothing.
            let _ = socket.write_all(&body[body.len() / 2..]).await;
            let _ = socket.shutdown().await;
        });
        let tool = fixture.tool(&endpoint, MAX_RESPONSE_BYTES, MAX_IMAGE_BYTES);
        let mut future = Box::pin(tool.execute(json!({"prompt": "x", "path": "late.png"})));
        assert!(
            tokio::time::timeout(Duration::from_millis(300), &mut future)
                .await
                .is_err(),
            "the tool must still be waiting for the partial body"
        );
        drop(future);
        let _ = complete_tx.send(());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!fixture.temp.path().join("late.png").exists());
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn symlink_escape_is_refused_without_an_outside_file() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), fixture.temp.path().join("escape")).unwrap();
        let (endpoint, server) = serve(image_response(&png_bytes())).await;
        let tool = fixture.tool(&endpoint, MAX_RESPONSE_BYTES, MAX_IMAGE_BYTES);
        let result = tool
            .execute(json!({"prompt": "x", "path": "escape/x.png"}))
            .await;
        assert!(result.is_err(), "{result:?}");
        assert!(!outside.path().join("x.png").exists());
        let _ = server.await;
    }

    #[test]
    fn generate_image_is_not_in_common_builtins() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(temp.path()).unwrap();
        let (main, _) =
            crate::tools::builtins_with_bash_timeout(workspace.clone(), None, false, None, None);
        assert!(!main.iter().any(|tool| tool.spec().name == "generate_image"));
        let child = crate::tools::builtins_with_background(
            workspace,
            BackgroundTasks::new(None, None),
            None,
            false,
            true,
            Some("sub".into()),
        );
        assert!(
            !child
                .iter()
                .any(|tool| tool.spec().name == "generate_image")
        );
    }
}
