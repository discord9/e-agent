//! Native Google Antigravity provider (`v1internal:streamGenerateContent`):
//! no CLI subprocess, one HTTP request per turn over the OAuth login the
//! Antigravity CLI (`agy`) already stored. The response is a JSON ARRAY
//! stream, decoded incrementally here so text parts reach the caller as
//! `ModelDeltaKind::Content` deltas. Refreshing an expired login also needs
//! the CLI's own OAuth consumer client (`client_id` / `client_secret` from
//! the provider's config): no credential ships inside e-agent.
//!
//! A tool call is a native `functionCall` part whose `thoughtSignature` MUST
//! be echoed back beside that call on the next request (the API answers 400
//! INVALID_ARGUMENT otherwise), so it rides in the opaque
//! [`ToolCall::provider_meta`] of THAT call; tool results are
//! `{"role":"user","parts":[{"functionResponse":{...}}]}`.
//!
//! The stream is framed as a JSON array: an opened-but-unclosed array, a bad
//! separator, trailing garbage, or an unfinished element is an error, never a
//! shorter successful turn (a truncated turn's tool calls must not run). Error
//! messages carry the HTTP status and an allowlisted provider error code only:
//! a provider body can quote the submitted credentials and those messages are
//! persisted in the session log.
//!
//! Tool declarations carry their raw JSON Schema in `parametersJsonSchema`,
//! not in the Schema-proto `parameters` field: the latter rejects `const`,
//! which the builtin `history` schema uses (400 INVALID_ARGUMENT, verified
//! live), and no downgrade of the builtin schemas is attempted.
//!
//! Non-goals: model listing (`fetchAvailableModels`), image input
//! (`supports_vision` is false), a retry framework (one 401 -> refresh ->
//! retry), and any `generationConfig` reasoning mapping (a `thinkingConfig` is
//! accepted with HTTP 200 but changed nothing observable, so
//! `reasoning_effort`/`thinking` are inert on this wire).

use anyhow::{Context, anyhow, bail};
use base64::Engine;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use futures_util::StreamExt;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::agent::{AssistantMessage, Message, Model, ModelDeltaKind, ToolCall, ToolSpec, Usage};
use crate::model::IncompleteToolCall;

/// Streaming generation endpoint on the Cloud Code "daily" host.
pub(crate) const STREAM_URL: &str =
    "https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent";
/// Google OAuth refresh endpoint.
pub(crate) const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// The only project id this endpoint accepts.
pub(crate) const PROJECT: &str = "aicode-consumers";
/// Byte-exact CLI user agent. `x-goog-api-client` must NOT be sent: it routes
/// the request into a "version no longer supported" canned reply.
pub(crate) const USER_AGENT: &str = "antigravity/cli/1.5.14 (aidev_client; os_type=linux; arch=amd64; cl=1026970498; auth_method=consumer)";

/// Refresh this long before the recorded expiry.
const EXPIRY_MARGIN: ChronoDuration = ChronoDuration::minutes(1);

/// One OAuth token set. `agy`'s file and e-agent's own cache share this
/// shape, so one reader serves both.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct TokenFile {
    token: StoredToken,
    /// [`login_fingerprint`] of the CLI refresh credential this cache entry
    /// was written for (`None` in `agy`'s own file, which has no such field).
    /// A cache whose fingerprint no longer matches the current CLI login
    /// belongs to a previous account and is ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cli_fingerprint: Option<String>,
}

/// Binding identity of a CLI login: the hash of its refresh credential, never
/// the credential itself, so recording it leaks nothing.
fn login_fingerprint(refresh_token: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(refresh_token.as_bytes()))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredToken {
    access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    /// RFC3339 absolute expiry (`agy` writes a local offset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expiry: Option<DateTime<Utc>>,
}

impl StoredToken {
    /// An unknown expiry counts as usable; the API's 401 is the backstop.
    fn usable(&self) -> bool {
        self.expiry
            .is_none_or(|expiry| expiry - Utc::now() > EXPIRY_MARGIN)
    }
}

#[derive(Clone)]
pub struct AntigravityModel {
    client: reqwest::Client,
    model: String,
    /// Kept for `/model` display only: configured but inert (see above).
    pub(crate) reasoning_effort: Option<String>,
    stream_url: String,
    token_endpoint: String,
    /// `agy`'s token file: READ-ONLY for e-agent.
    token_file: PathBuf,
    /// e-agent's own writable token cache (0600).
    cache_file: PathBuf,
    /// The `agy` install's OAuth consumer client, from the provider's
    /// `client_id` / `client_secret`: only a refresh spends it, and no
    /// default is assumed (a valid access token needs neither).
    client_id: Option<String>,
    client_secret: Option<String>,
}

impl AntigravityModel {
    pub fn new(
        model: String,
        reasoning_effort: Option<String>,
        client_id: Option<String>,
        client_secret: Option<String>,
    ) -> anyhow::Result<Self> {
        let home = crate::home_dir().ok_or_else(|| anyhow!("cannot locate the home directory"))?;
        let config_dir = crate::config::config_dir()
            .ok_or_else(|| anyhow!("cannot locate the e-agent config directory"))?;
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .build()
                .context("cannot create HTTP client")?,
            model,
            reasoning_effort,
            stream_url: STREAM_URL.into(),
            token_endpoint: TOKEN_URL.into(),
            token_file: home.join(".gemini/antigravity-cli/antigravity-oauth-token"),
            cache_file: config_dir.join("antigravity-token.json"),
            client_id,
            client_secret,
        })
    }

    /// Model slug (the `fetchAvailableModels` id), used for display.
    pub fn name(&self) -> &str {
        &self.model
    }

    /// `agy`'s login plus e-agent's cache when (and only when) that cache is
    /// bound to it: a cache whose [`TokenFile::cli_fingerprint`] differs from
    /// the current CLI refresh credential was written for a previous `agy`
    /// login and is ignored, so a re-login can never keep spending the old
    /// account. A corrupt/missing CLI file is an error rather than masked by
    /// the cache — without the CLI credential e-agent cannot tell which
    /// account the cache belongs to, and `agy` is one command away.
    fn credentials(&self) -> anyhow::Result<(Option<StoredToken>, StoredToken)> {
        let cli = read_token(&self.token_file)?.ok_or_else(|| {
            anyhow!(
                "Antigravity CLI token file {} is missing; run `agy` to log in",
                self.token_file.display()
            )
        })?;
        let fingerprint = cli.token.refresh_token.as_deref().map(login_fingerprint);
        let cached = read_token(&self.cache_file)
            .ok()
            .flatten()
            .filter(|cache| cache.cli_fingerprint.is_some() && cache.cli_fingerprint == fingerprint)
            .map(|cache| cache.token);
        Ok((cached, cli.token))
    }

    /// A token to spend now: a stored one while still usable (the bound cache
    /// first, then the CLI's), else a refresh.
    async fn access_token(&mut self) -> anyhow::Result<String> {
        let (cached, cli) = self.credentials()?;
        match [cached, Some(cli)]
            .into_iter()
            .flatten()
            .find(StoredToken::usable)
        {
            Some(token) => Ok(token.access_token),
            None => self.refresh().await,
        }
    }

    /// Exchange the refresh credential of the SELECTED login for a fresh
    /// access token and cache it in e-agent's own config dir, bound to that
    /// login's fingerprint. The CLI's file is never written.
    async fn refresh(&mut self) -> anyhow::Result<String> {
        let (cached, cli) = self.credentials()?;
        let cli_refresh = cli.refresh_token.clone().ok_or_else(|| {
            anyhow!(
                "Antigravity CLI token file {} has no refresh_token; run `agy` to log in again",
                self.token_file.display()
            )
        })?;
        // The bound cache may hold a rotated refresh token for the same login;
        // an unbound cache was already dropped by `credentials`.
        let refresh_token = cached
            .and_then(|token| token.refresh_token)
            .unwrap_or_else(|| cli_refresh.clone());
        // The consumer client that may spend this credential is the `agy`
        // install's own: it is configured, never compiled in.
        let (client_id, client_secret) = match (&self.client_id, &self.client_secret) {
            (Some(id), Some(secret)) if !id.trim().is_empty() && !secret.trim().is_empty() => {
                (id.as_str(), secret.as_str())
            }
            _ => bail!(
                "Antigravity cannot refresh the login: set `client_id` and `client_secret` on the provider in the config, copied from the `agy` install (its OAuth consumer client); a still-valid access token from `agy` works without them"
            ),
        };
        let response = self
            .client
            .post(&self.token_endpoint)
            .form(&[
                ("client_id", client_id),
                ("client_secret", client_secret),
                ("refresh_token", refresh_token.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await
            .context("Antigravity token refresh request failed")?;
        let status = response.status();
        // The body is never surfaced: a rejected refresh echoes account
        // material and must not reach logs or tool output.
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            // A definitive rejection means the credential used here is dead.
            // Evict it so the NEXT request starts over from the CLI's own
            // credential (no retry happens inside this turn) and say what to
            // do — but only if the cache still holds THAT credential: another
            // refresh may have installed a newer entry meanwhile, and deleting
            // it would throw away a working login.
            if body_error_code(&body) == Some("invalid_grant") {
                evict_rejected_cache(&self.cache_file, &refresh_token).await;
                bail!(
                    "Antigravity rejected the login credential (invalid_grant); run `agy` to log in again"
                );
            }
            bail!("Antigravity token refresh failed with HTTP {status}");
        }
        let refreshed: Value = serde_json::from_str(&body).map_err(|error| {
            // A decode error can quote the input; this one is about a
            // credential-bearing body, so only the position is reported.
            anyhow!(
                "cannot decode the Antigravity token refresh (line {}, column {})",
                error.line(),
                error.column()
            )
        })?;
        let token = StoredToken {
            access_token: refreshed["access_token"]
                .as_str()
                .context("Antigravity refresh response has no access_token")?
                .to_owned(),
            refresh_token: Some(
                refreshed["refresh_token"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or(refresh_token),
            ),
            expiry: refreshed["expires_in"]
                .as_u64()
                .map(|secs| Utc::now() + ChronoDuration::seconds(secs as i64)),
        };
        // Best effort: a read-only config dir must not fail the turn (the
        // returned token still covers this request).
        if let Err(error) = replace_cache(
            &self.cache_file,
            &TokenFile {
                token: token.clone(),
                cli_fingerprint: Some(login_fingerprint(&cli_refresh)),
            },
        )
        .await
        {
            // Not fatal, but the operator should know why every turn refreshes.
            tracing::warn!("cannot cache the Antigravity token: {error:#}");
        }
        Ok(token.access_token)
    }

    /// The full wire body for one turn. Fails when the tool batch being
    /// continued now carries no `thoughtSignature` at all: the API answers 400
    /// ("Function call is missing a thought_signature in functionCall parts",
    /// verified live), so the turn is refused locally, naming the call,
    /// instead of a signature being invented for it.
    fn request_body(&self, messages: &[Message], tools: &[ToolSpec]) -> anyhow::Result<Value> {
        let system = messages
            .iter()
            .filter_map(|message| match message {
                Message::System { content } => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        // Only the tool batch being continued NOW needs its signature: the API
        // validates the step whose results are being sent. That step is the
        // trailing, still-OPEN batch — a run of tool results at the very end of
        // history plus the assistant turn that asked for them. A batch a later
        // assistant turn has already answered, and a history that ends with a
        // user prompt or a final answer, have nothing to validate and replay
        // exactly as recorded (a resumed or forked conversation must not be
        // refused for history it merely carries).
        let trailing_results = messages
            .iter()
            .rev()
            .take_while(|message| matches!(message, Message::Tool { .. }))
            .count();
        let active_batch = if trailing_results == 0 {
            None
        } else {
            messages[..messages.len() - trailing_results]
                .iter()
                .rposition(|message| {
                    matches!(message, Message::Assistant(assistant) if !assistant.tool_calls.is_empty())
                })
        };
        let mut contents: Vec<Value> = Vec::new();
        for (index, message) in messages.iter().enumerate() {
            let (role, parts) = match message {
                Message::System { .. } => continue,
                // Text only: `supports_vision` is false, so the agent's vision
                // gate rejects attached images before this wire is reached.
                Message::User { content, .. } => ("user", vec![json!({"text": content})]),
                Message::Assistant(message) => {
                    let mut parts: Vec<Value> = Vec::new();
                    if let Some(text) = message.content.as_deref().filter(|text| !text.is_empty()) {
                        parts.push(json!({"text": text}));
                    }
                    // The active batch's signature must ride back with its
                    // call parts (a turn with none is the verified 400), but a
                    // call that arrived unsigned stays unsigned: one call's
                    // signature is never copied onto another.
                    if Some(index) == active_batch
                        && !message
                            .tool_calls
                            .iter()
                            .any(|call| call.provider_meta.is_some())
                    {
                        let first = &message.tool_calls[0];
                        return Err(anyhow!(
                            "Antigravity tool call `{}` ({}) and its siblings carry no thoughtSignature, so this history cannot be replayed on this wire; start a new session or switch back to the model that produced it",
                            first.id,
                            first.name
                        ));
                    }
                    for call in &message.tool_calls {
                        let mut part = json!({
                            "functionCall": {
                                "name": call.name,
                                "args": call_arguments(&call.arguments),
                                "id": call.id,
                            },
                        });
                        if let Some(signature) = &call.provider_meta {
                            part["thoughtSignature"] = json!(signature);
                        }
                        parts.push(part);
                    }
                    if parts.is_empty() {
                        continue;
                    }
                    ("model", parts)
                }
                Message::Tool {
                    call_id,
                    name,
                    content,
                    is_error,
                    ..
                } => (
                    "user",
                    vec![json!({"functionResponse": {
                        "name": name,
                        "id": call_id,
                        "response": {"output": if *is_error {
                            format!("ERROR: {content}")
                        } else {
                            content.clone()
                        }},
                    }})],
                ),
            };
            // Repeated same-role turns merge into one content: a tool batch is
            // a single user turn on this wire.
            match contents.last_mut() {
                Some(last) if last["role"] == role => {
                    if let Some(existing) = last["parts"].as_array_mut() {
                        existing.extend(parts);
                    }
                }
                _ => contents.push(json!({"role": role, "parts": parts})),
            }
        }
        let mut request = json!({"contents": contents, "generationConfig": {}});
        if !system.is_empty() {
            request["systemInstruction"] = json!({"parts": [{"text": system}]});
        }
        if !tools.is_empty() {
            let declarations = tools
                .iter()
                .map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        // Raw JSON Schema: the `parameters` field is a Schema
                        // proto that REJECTS `const` (the builtin `history`
                        // tool uses it: HTTP 400 INVALID_ARGUMENT, verified
                        // live), while `parametersJsonSchema` accepts the
                        // builtin schemas and the model invokes them.
                        "parametersJsonSchema": tool.parameters,
                    })
                })
                .collect::<Vec<_>>();
            request["tools"] = json!([{"functionDeclarations": declarations}]);
        }
        Ok(json!({"project": PROJECT, "model": self.model, "request": request}))
    }

    async fn send(&self, token: &str, request: &Value) -> anyhow::Result<reqwest::Response> {
        self.client
            .post(&self.stream_url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .bearer_auth(token)
            .json(request)
            .send()
            .await
            .with_context(|| format!("Antigravity request failed (model={})", self.model))
    }
}

#[async_trait::async_trait]
impl Model for AntigravityModel {
    fn name(&self) -> &str {
        self.name()
    }

    // Text-only: `supports_vision` keeps the trait default (false).

    async fn complete(
        &mut self,
        messages: &[Message],
        tools: &[ToolSpec],
        on_delta: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        crate::agent::ensure_vision_supported(&self.model, false, messages)?;
        let request = self.request_body(messages, tools)?;
        let token = self.access_token().await?;
        let mut response = self.send(&token, &request).await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            // One bounded recovery: refresh once, then retry the same body.
            let token = self.refresh().await?;
            response = self.send(&token, &request).await?;
        }
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            // Never the body: a provider error can quote the submitted
            // credentials and this message is persisted in the session log.
            let code = body_error_code(&body)
                .map(|code| format!(" ({code})"))
                .unwrap_or_default();
            bail!(
                "Antigravity returned HTTP {status} (model={}){code}",
                self.model
            );
        }
        let mut discard = |_kind: ModelDeltaKind, _text: &str| {};
        let emit: &mut (dyn FnMut(ModelDeltaKind, &str) + Send) = match on_delta {
            Some(callback) => callback,
            None => &mut discard,
        };
        let mut scanner = ArrayStream::default();
        let mut turn = Turn::default();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("cannot read the Antigravity response stream")?;
            for element in scanner.push(&chunk)? {
                turn.absorb(&element, &mut *emit)?;
            }
        }
        // A clean EOF is not a success on its own: the array must have closed.
        scanner.finish()?;
        turn.finish()
    }
}

#[derive(Clone, Copy, Default, Deserialize)]
struct UsageMetadata {
    #[serde(default, rename = "promptTokenCount")]
    prompt: u64,
    #[serde(default, rename = "candidatesTokenCount")]
    candidates: u64,
    #[serde(default, rename = "thoughtsTokenCount")]
    thoughts: Option<u64>,
}

/// The accumulated turn plus the two wire quirks worth naming: usage settles
/// on the LAST element (earlier ones repeat partial counts) and a
/// `thoughtSignature` may be its own part immediately after its call.
#[derive(Default)]
struct Turn {
    content: String,
    tool_calls: Vec<ToolCall>,
    usage: Option<UsageMetadata>,
    finish_reason: Option<String>,
}

impl Turn {
    fn absorb(
        &mut self,
        element: &Value,
        on_delta: &mut (dyn FnMut(ModelDeltaKind, &str) + Send),
    ) -> anyhow::Result<()> {
        // A mid-stream failure still arrives inside the array shape. Only the
        // allowlisted code is surfaced (the body can quote credentials).
        if let Some(error) = element.get("error").filter(|error| !error.is_null()) {
            let code = error_code(error)
                .map(|code| format!(" ({code})"))
                .unwrap_or_default();
            bail!("Antigravity stream error{code}");
        }
        let response = &element["response"];
        if let Some(metadata) = response
            .get("usageMetadata")
            .and_then(|value| serde_json::from_value(value.clone()).ok())
        {
            self.usage = Some(metadata);
        }
        for candidate in response["candidates"].as_array().into_iter().flatten() {
            if let Some(reason) = candidate["finishReason"].as_str() {
                self.finish_reason = Some(reason.to_owned());
            }
            let parts = candidate["content"]["parts"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            // A signature belongs to the part it arrived with: the call
            // itself, or a signature-only part sitting DIRECTLY after its call
            // in the same element. Nothing is copied across parts, and any
            // intervening part breaks the association.
            let mut signed_call: Option<usize> = None;
            for part in parts {
                if let Some(call) = part.get("functionCall") {
                    let name = call["name"].as_str().unwrap_or_default();
                    if name.is_empty() {
                        return Err(anyhow::Error::new(IncompleteToolCall));
                    }
                    let id = call["id"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .map(str::to_owned)
                        .unwrap_or_else(local_call_id);
                    if self.tool_calls.iter().any(|existing| existing.id == id) {
                        bail!("Antigravity stream repeated tool call id `{id}` in one turn");
                    }
                    self.tool_calls.push(ToolCall {
                        id,
                        name: name.to_owned(),
                        arguments: call
                            .get("args")
                            .map(Value::to_string)
                            .unwrap_or_else(|| "{}".into()),
                        provider_meta: part["thoughtSignature"].as_str().map(str::to_owned),
                    });
                    signed_call = Some(self.tool_calls.len() - 1);
                    continue;
                }
                // Text is text, whatever else the part carries: a nonempty
                // text emits its own delta and ends the adjacency, so a
                // signature stamped on it can never sign the call before it.
                // Provider thought text is a REASONING delta and never becomes
                // the assistant content (it is never replayed).
                if let Some(text) = part["text"].as_str().filter(|text| !text.is_empty()) {
                    if part["thought"].as_bool().unwrap_or(false) {
                        on_delta(ModelDeltaKind::Reasoning, text);
                    } else {
                        self.content.push_str(text);
                        on_delta(ModelDeltaKind::Content, text);
                    }
                    signed_call = None;
                    continue;
                }
                // Signature-only: no text AT ALL (absent or empty) and no
                // call. This is the shape the wire uses when the signature is
                // its own part rather than a field of the call.
                if let Some(signature) = part["thoughtSignature"].as_str() {
                    if let Some(call) = signed_call.and_then(|index| self.tool_calls.get_mut(index))
                    {
                        call.provider_meta
                            .get_or_insert_with(|| signature.to_owned());
                    }
                    signed_call = None;
                    continue;
                }
                // Any other part — an empty text, an unknown field —
                // INTERVENES: a signature part after it no longer sits
                // directly after its call.
                signed_call = None;
            }
        }
        Ok(())
    }

    fn finish(self) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        if self.content.is_empty() && self.tool_calls.is_empty() {
            bail!("Antigravity stream ended without content or tool calls (incomplete turn)");
        }
        let usage = self.usage.map(|metadata| Usage {
            input_tokens: metadata.prompt,
            // Thinking tokens are generated output the caller pays for.
            output_tokens: metadata.candidates + metadata.thoughts.unwrap_or(0),
            cache_hit_tokens: None,
            cache_miss_tokens: None,
            reasoning_tokens: metadata.thoughts,
            finish_reason: self.finish_reason,
        });
        Ok((
            AssistantMessage {
                content: (!self.content.is_empty()).then_some(self.content),
                tool_calls: self.tool_calls,
                reasoning: None,
            },
            usage,
        ))
    }
}

/// Incremental reader for a JSON ARRAY stream (`[{"response":..}\n,{..}]`):
/// keeps the undecoded tail and yields each complete top-level object as soon
/// as its closing brace arrives. The framing itself is checked as it arrives —
/// an opened array, `,` between elements, a closing `]`, whitespace after it —
/// because the service also closes an ABORTED stream cleanly: a bare
/// "count balanced braces" scan would hand a truncated turn back as a success
/// and the runner would execute its tool calls.
#[derive(Default)]
struct ArrayStream {
    pending: Vec<u8>,
    pos: usize,
    depth: usize,
    in_string: bool,
    escape: bool,
    start: Option<usize>,
    frame: Frame,
}

/// Framing state BETWEEN elements.
#[derive(Clone, Copy, Default, PartialEq)]
enum Frame {
    /// Before the opening `[`.
    #[default]
    Opened,
    /// Directly after `[`: `]` (empty array) or an element.
    AfterOpen,
    /// After `,`: an element must come (a trailing `,` is not JSON).
    Element,
    /// After an element: `,` or `]`.
    Separator,
    /// After `]`: whitespace only.
    Closed,
}

/// The four bytes JSON allows between tokens (space, tab, CR, LF). Notably not
/// `is_ascii_whitespace()`, which also admits a form feed.
fn is_json_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

impl ArrayStream {
    fn push(&mut self, chunk: &[u8]) -> anyhow::Result<Vec<Value>> {
        self.pending.extend_from_slice(chunk);
        let mut elements = Vec::new();
        while self.pos < self.pending.len() {
            let byte = self.pending[self.pos];
            if self.in_string {
                if self.escape {
                    self.escape = false;
                } else if byte == b'\\' {
                    self.escape = true;
                } else if byte == b'"' {
                    self.in_string = false;
                }
            } else if self.depth > 0 {
                match byte {
                    b'"' => self.in_string = true,
                    b'{' => self.depth += 1,
                    b'}' => {
                        self.depth -= 1;
                        if self.depth == 0
                            && let Some(start) = self.start.take()
                        {
                            elements.push(
                                serde_json::from_slice(&self.pending[start..=self.pos])
                                    .context("cannot decode an Antigravity stream element")?,
                            );
                            self.frame = Frame::Separator;
                        }
                    }
                    _ => {}
                }
            } else {
                match byte {
                    whitespace if is_json_whitespace(whitespace) => {}
                    b'[' if self.frame == Frame::Opened => self.frame = Frame::AfterOpen,
                    b'{' if matches!(self.frame, Frame::AfterOpen | Frame::Element) => {
                        self.start = Some(self.pos);
                        self.depth = 1;
                    }
                    b',' if self.frame == Frame::Separator => self.frame = Frame::Element,
                    // `[]` is the only state that may close without an
                    // element; after a `,` a `]` is a trailing comma.
                    b']' if matches!(self.frame, Frame::AfterOpen | Frame::Separator) => {
                        self.frame = Frame::Closed
                    }
                    // The offset only: the byte may be part of a credential-
                    // bearing body and this message is persisted.
                    _ => bail!("Antigravity stream framing is invalid at byte {}", self.pos),
                }
            }
            self.pos += 1;
        }
        // Keep the undecoded tail, starting at the open element's `{`.
        let consumed = self.start.unwrap_or(self.pos);
        self.pending.drain(..consumed);
        self.pos -= consumed;
        if let Some(start) = self.start.as_mut() {
            *start -= consumed;
        }
        Ok(elements)
    }

    /// Called once at a clean EOF: the array must have opened AND closed, with
    /// no unfinished element and nothing but whitespace behind it.
    fn finish(&mut self) -> anyhow::Result<()> {
        let clean = self.frame == Frame::Closed
            && self.depth == 0
            && self.start.is_none()
            && !self.in_string
            && self.pending.iter().copied().all(is_json_whitespace);
        if !clean {
            bail!(
                "Antigravity response stream ended before its JSON array closed (truncated turn)"
            );
        }
        Ok(())
    }
}

/// `functionCall.args` is an object while e-agent keeps raw argument text;
/// anything that is not an object is wrapped rather than dropped.
fn call_arguments(arguments: &str) -> Value {
    match serde_json::from_str::<Value>(arguments) {
        Ok(value @ Value::Object(_)) => value,
        Ok(other) => json!({"input": other}),
        Err(_) => json!({"input": arguments}),
    }
}

/// A unique, persisted id for a call the wire did not identify itself: pending
/// questions are keyed by call id, so a per-turn counter would repeat across
/// turns.
fn local_call_id() -> String {
    format!("e-agent-call-{:016x}", rand::random::<u64>())
}

/// The only provider codes e-agent will ever repeat: the gRPC canonical status
/// names, the numeric statuses this endpoint mirrors into `code`, and the few
/// reason codes it is known to answer with. A value is matched against these
/// EXACT constants and the constant is what gets printed — never the body's
/// own text, because a token-shaped string can arrive in `status`, `code`,
/// `type`, or `details[].reason` and these messages are persisted in session
/// history.
const ERROR_CODES: &[&str] = &[
    // gRPC canonical statuses.
    "INVALID_ARGUMENT",
    "PERMISSION_DENIED",
    "RESOURCE_EXHAUSTED",
    "UNAUTHENTICATED",
    "NOT_FOUND",
    "FAILED_PRECONDITION",
    "INTERNAL",
    "UNAVAILABLE",
    "DEADLINE_EXCEEDED",
    "ABORTED",
    "OUT_OF_RANGE",
    "UNIMPLEMENTED",
    "ALREADY_EXISTS",
    "CANCELLED",
    "DATA_LOSS",
    "UNKNOWN",
    // HTTP statuses mirrored into `code`.
    "400",
    "401",
    "403",
    "404",
    "429",
    "500",
    "502",
    "503",
    "504",
    // Reasons this wire is known to name.
    "API_KEY_INVALID",
    "API_KEY_EXPIRED",
    "invalid_grant",
];

/// The allowlisted constant equal to `candidate`, if any.
fn allowlisted(candidate: &str) -> Option<&'static str> {
    ERROR_CODES.iter().copied().find(|code| *code == candidate)
}

/// The allowlisted code of an error object (or of the whole element carrying
/// one). Anything not in [`ERROR_CODES`] yields `None`, so the caller reports
/// the HTTP status alone.
fn error_code(error: &Value) -> Option<&'static str> {
    let error = error.get("error").unwrap_or(error);
    // The OAuth shape is a bare string: `{"error":"invalid_grant",..}`.
    if let Some(text) = error.as_str() {
        return allowlisted(text);
    }
    ["status", "code", "type"]
        .into_iter()
        .filter_map(|field| error[field].as_str())
        .chain(error["details"][0]["reason"].as_str())
        .filter_map(allowlisted)
        .next()
}

/// [`error_code`] for an undecoded body, which the wire frames as a bare
/// object, as the first element of an array, or as an OAuth error document.
fn body_error_code(body: &str) -> Option<&'static str> {
    let value: Value = serde_json::from_str(body).ok()?;
    let value = value
        .as_array()
        .and_then(|array| array.first())
        .unwrap_or(&value);
    error_code(value)
}

fn read_token(path: &Path) -> anyhow::Result<Option<TokenFile>> {
    match std::fs::read(path) {
        Ok(source) => Ok(Some(serde_json::from_slice::<TokenFile>(&source).map_err(
            |error| {
                // A decode error can quote the input; a credential file's
                // contents must not reach the session log.
                anyhow!(
                    "cannot decode {} (line {}, column {})",
                    path.display(),
                    error.line(),
                    error.column()
                )
            },
        )?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Serializes cache REPLACEMENT against conditional EVICTION, in this process.
/// Without it the "read the entry, compare it, unlink it" sequence could run
/// between another refresh's compare and its rename, deleting a credential
/// that refresh had just installed. Cross-process races are out of scope: the
/// replacement itself is an atomic rename, so a second e-agent process can at
/// worst lose its cache entry (one extra refresh), never a login.
static CACHE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Install a refreshed token, serialized with [`evict_rejected_cache`].
async fn replace_cache(path: &Path, file: &TokenFile) -> anyhow::Result<()> {
    let _guard = CACHE_LOCK.lock().await;
    write_token(path, file)
}

/// Drop the cache entry, but only if it still holds the credential that was
/// just rejected. The re-read happens UNDER [`CACHE_LOCK`], so a newer entry
/// another refresh installed (before this point, or queued behind this one) is
/// never deleted. Never called with the lock held and never held across I/O
/// that talks to the network.
async fn evict_rejected_cache(path: &Path, rejected: &str) {
    let _guard = CACHE_LOCK.lock().await;
    let stale = read_token(path)
        .ok()
        .flatten()
        .is_some_and(|cache| cache.token.refresh_token.as_deref() == Some(rejected));
    if stale {
        let _ = std::fs::remove_file(path);
    }
}

/// Replace e-agent's own cache atomically: write a private temp file in the
/// destination directory, tighten it, fsync it, then rename it over the
/// destination. An interrupted write therefore cannot truncate the stored
/// credential, an existing permissive file is replaced (not reused), and a
/// symlink at the destination is replaced rather than written through.
/// Owner-only 0600 is set on the OPEN HANDLE (a pathname chmod could follow a
/// link swapped in later) and is a Unix promise; on Windows the file inherits
/// the directory ACL. The directory itself is not fsynced after the rename:
/// the replacement is atomic per the OS, its durability across a power loss is
/// left to the filesystem journal — a documented limit, not a guarantee.
fn write_token(path: &Path, file: &TokenFile) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("invalid Antigravity cache path"))?;
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let mut bytes = serde_json::to_vec(file)?;
    bytes.push(b'\n');
    let (temporary, mut handle) = create_private_temp(dir)?;
    let write = (|| -> anyhow::Result<()> {
        handle.write_all(&bytes)?;
        #[cfg(unix)]
        handle.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        handle.sync_all()?;
        Ok(())
    })();
    drop(handle);
    let result = write.and_then(|()| {
        std::fs::rename(&temporary, path)
            .with_context(|| format!("cannot replace {}", path.display()))?;
        Ok(())
    });
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn create_private_temp(dir: &Path) -> anyhow::Result<(PathBuf, std::fs::File)> {
    for _ in 0..16 {
        let mut suffix = [0u8; 16];
        rand::rng().fill_bytes(&mut suffix);
        let path = dir.join(format!(
            ".antigravity-token.{}.tmp",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(suffix)
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "cannot create a private token temporary in {}",
                        dir.display()
                    )
                });
            }
        }
    }
    bail!("cannot create a unique private token temporary")
}

#[cfg(test)]
mod tests;
