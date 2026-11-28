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
//! The stream is Server-Sent Events (`?alt=sse`): `data: {json}` events, one
//! blank line between them, the same `{"response": ..}` envelope either way
//! (verified live). An EOF with a half-read line or an undispatched event is an
//! error, never a shorter successful turn (a truncated turn's tool calls must
//! not run). A complete stream is not a completed generation either: a batch of
//! executable calls needs a terminal `STOP` — a `MAX_TOKENS` answer is still
//! displayed as TEXT (a truncated answer is visible as one), while a truncated
//! or provider-marked FAILED batch is refused, as is a batch whose first call
//! carries no `thoughtSignature` (it could never be replayed, so no call in it
//! is run). A TEXT-only turn may end without a `finishReason` at all: the live
//! stream does not always send one, and text has no side effect — truncation is
//! caught by the framing instead.
//!
//! A request carries the CLI's own envelope (`requestType`, `requestId`, and a
//! conversation-derived `request.sessionId`), is repaired rather than refused
//! where the wire tolerates it (empty parts, missing call/response ids, a
//! drifted response name, a missing tool schema), and a 404 for a bare flash
//! model is retried ONCE against that family's shared `-tiered` endpoint: a
//! pinned client version is only allowed to reach that one.
//! Error messages carry the HTTP status, an allowlisted provider error code,
//! and allowlisted `finishReason` constants only: a provider body can quote the
//! submitted credentials and those messages are persisted in the session log.
//! No redirect is ever followed, on either POST: the refresh body (client
//! secret and refresh token) is replayable and the generation body carries the
//! conversation.
//!
//! Tool declarations carry their raw JSON Schema in `parametersJsonSchema`,
//! not in the Schema-proto `parameters` field: the latter rejects `const`,
//! which the builtin `history` schema uses (400 INVALID_ARGUMENT, verified
//! live), and no downgrade of the builtin schemas is attempted.
//!
//! If this wire ever answers a bare 429 with no explanation, look at the
//! `systemInstruction` text FIRST. A sibling implementation bisected seven
//! verbatim prompt lines that produced `429 RESOURCE_EXHAUSTED` while the
//! account still had quota (9router, 2026-11-18), and it probed that the same
//! text sent as a user turn passes — the identity is fingerprinted where it
//! sits in `systemInstruction`, and everything this session sends goes there.
//! None of those seven lines occurs in e-agent's prompt today (checked
//! verbatim, not by resemblance), so this is a diagnostic hint, not a known
//! hazard; header order and body field order are what other clients align,
//! but no source shows a 429 flipping on those alone.
//!
//! The account quota card is the ONE extra call ([`quota_summary`]):
//! read-only, only when the quota surface asks, never during a turn.
//!
//! Non-goals: model listing (`fetchAvailableModels`), image input
//! (`supports_vision` is false), a retry framework (one 401 -> refresh ->
//! retry, and one 404 -> `-tiered` fallback, never a loop), and any
//! `generationConfig` reasoning mapping (a `thinkingConfig` is
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
use std::time::{Duration, Instant};

use crate::agent::{
    AssistantMessage, ImagePart, Message, Model, ModelDeltaKind, ToolCall, ToolSpec, Usage,
};
use crate::model::IncompleteToolCall;

/// Streaming generation endpoint on the Cloud Code "daily" host, in its SSE
/// form (`alt=sse`): one `data: {json}` event per chunk instead of the
/// default JSON-array framing, which needs no state machine of ours. Same
/// envelope either way (verified live).
pub(crate) const STREAM_URL: &str =
    "https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse";
/// Google OAuth refresh endpoint.
pub(crate) const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// Account quota summary: a `POST` with `{"project": PROJECT}` answers
/// `groups[].buckets[]` of `{bucketId, window, remainingFraction, resetTime,
/// disabled}` for the logged-in account. Read-only.
pub(crate) const QUOTA_URL: &str =
    "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary";
/// The only project id this endpoint accepts.
pub(crate) const PROJECT: &str = "aicode-consumers";
/// `requestType` every CLI generation request carries.
pub(crate) const REQUEST_TYPE: &str = "agent";
/// Body-level `userAgent` field the CLI sends beside `requestType` (the HTTP
/// header is separate): reference clients send the short string `antigravity`.
pub(crate) const REQUEST_USER_AGENT: &str = "antigravity";
/// The shared tier endpoint suffix: what a pinned client version is allowed to
/// reach when the backend refuses the bare/`-high|-medium|-low` flash id.
pub(crate) const TIERED_SUFFIX: &str = "-tiered";
/// Byte-exact CLI user agent. `x-goog-api-client` must NOT be sent: it routes
/// the request into a "version no longer supported" canned reply.
pub(crate) const USER_AGENT: &str = "antigravity/cli/1.5.14 (aidev_client; os_type=linux; arch=amd64; cl=1026970498; auth_method=consumer)";

/// Refresh this long before the recorded expiry.
const EXPIRY_MARGIN: ChronoDuration = ChronoDuration::minutes(1);

/// Default minimum spacing between request STARTS on one model: 1 request per
/// second. This is a TERMS-RISK mitigation, not a quota guarantee: prolonged
/// machine-speed use of an interactive OAuth session is what gets accounts
/// suspended, and 60 RPM is the upper bound a third-party routing project calls
/// defensible for sustained use. `min_interval_ms = 0` disables it.
pub(crate) const DEFAULT_MIN_INTERVAL_MS: u64 = 1000;

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
    /// Minimum spacing between request STARTS (zero = pacing disabled), from
    /// the provider's `min_interval_ms`.
    min_interval: Duration,
    /// Start time of the last request, shared by clones: the pacer's only
    /// state. Never held across a network call (see [`Self::pace`]).
    last_request: std::sync::Arc<tokio::sync::Mutex<Option<Instant>>>,
    /// `<conversation>` of every `requestId`: derived from the FIRST turn's
    /// contents and then frozen, so each turn of the session — and every clone
    /// of this model — sends the same conversation segment.
    conversation: std::sync::Arc<std::sync::OnceLock<String>>,
    /// Whether this profile takes images (profile `vision`, default TRUE).
    /// `supports_vision` reports it and `complete` gates on it.
    vision: bool,
    /// Content-addressed image store: BYTES are read at send time and never
    /// persisted or logged (see [`Self::inline_parts`]).
    image_store: Option<PathBuf>,
}

impl AntigravityModel {
    /// The one client shape for both POSTs: the generation request and the
    /// OAuth refresh. Redirects are DISABLED — a 3xx is a status-only failure.
    /// The refresh sends `client_secret` and `refresh_token` in a replayable
    /// form body and the generation POST carries the conversation, so neither
    /// may be re-sent to a URL that a response names.
    fn client_builder() -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(600))
    }

    pub fn new(
        model: String,
        reasoning_effort: Option<String>,
        client_id: Option<String>,
        client_secret: Option<String>,
        min_interval_ms: Option<u64>,
    ) -> anyhow::Result<Self> {
        let home = crate::home_dir().ok_or_else(|| anyhow!("cannot locate the home directory"))?;
        let config_dir = crate::config::config_dir()
            .ok_or_else(|| anyhow!("cannot locate the e-agent config directory"))?;
        Ok(Self {
            client: Self::client_builder()
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
            min_interval: Duration::from_millis(min_interval_ms.unwrap_or(DEFAULT_MIN_INTERVAL_MS)),
            last_request: Default::default(),
            conversation: Default::default(),
            vision: true,
            image_store: crate::agent::image_store_dir(),
        })
    }

    /// The two ids a `requestId` carries: the session's CONVERSATION id
    /// (UUID-shaped, derived from the first turn's contents and then frozen for
    /// this model instance) and this TURN's trajectory id (derived from this
    /// turn's contents, so a retry reuses it and the next turn does not).
    fn turn_ids(&self, request: &Value) -> (String, String) {
        let contents = request["request"]["contents"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let digest = conversation_digest(contents);
        let conversation = self.conversation.get_or_init(|| uuid_like(digest)).clone();
        (conversation, uuid_like(digest))
    }

    /// Reserve the next request slot: read the previous start under the lock,
    /// store this one, and RELEASE the lock before sleeping, so the network
    /// call never holds it. Taking the decision under the lock is what makes
    /// concurrent callers serialize — two parallel rounds cannot both pass at
    /// once, and each one waits its own remainder. Every request (the
    /// generation POST, a refresh, the post-401 retry) goes through here.
    async fn pace(&self) {
        if self.min_interval.is_zero() {
            return;
        }
        let slot = {
            let mut last = self.last_request.lock().await;
            let now = Instant::now();
            // The interval is measured from the previous request's START, and
            // that start may still be in the future (another caller already
            // reserved it): take the first slot at least one interval later.
            let slot = last.map_or(now, |previous| now.max(previous + self.min_interval));
            *last = Some(slot);
            slot
        };
        let wait = slot.saturating_duration_since(Instant::now());
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    /// Whether this profile takes image input (profile `vision`, default
    /// TRUE; `vision = false` marks a text-only model).
    pub fn with_vision(mut self, vision: bool) -> Self {
        self.vision = vision;
        self
    }

    /// Point the image reader at an explicit store (tests); the default is the
    /// global content-addressed one.
    #[cfg(test)]
    pub(crate) fn with_image_store(mut self, store: PathBuf) -> Self {
        self.image_store = Some(store);
        self
    }

    /// `inlineData` parts for these images, read from the content-addressed
    /// store at SEND time (the same `agent::load_image_bytes` helper the chat
    /// wire uses): the bytes are never persisted, cached, or logged — only the
    /// hash is. A missing file degrades to a `[image missing: <hash>]` text
    /// part instead of failing, exactly like the chat wire.
    fn inline_parts(&self, images: &[ImagePart]) -> Vec<Value> {
        images
            .iter()
            .map(|image| {
                match crate::agent::load_image_bytes(self.image_store.as_deref(), &image.hash) {
                    Some(bytes) => json!({"inlineData": {
                        "mimeType": image.mime,
                        "data": base64::engine::general_purpose::STANDARD.encode(bytes),
                    }}),
                    None => json!({"text": format!("[image missing: {}]", image.hash)}),
                }
            })
            .collect()
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
        // A refresh is a request too: it is paced like any other, not
        // differently and not around.
        self.pace().await;
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
        // A redirect is a status-only failure: the redirect policy is off, so
        // this is the status the endpoint itself answered with, and its body
        // (which names another URL) is not read for meaning — nothing is
        // evicted on a 3xx and no `Location` is logged or followed.
        if status.is_redirection() {
            bail!(
                "Antigravity token refresh was redirected (HTTP {status}); redirects are not followed"
            );
        }
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
                Message::User { content, images } => {
                    let mut parts = vec![json!({"text": content})];
                    parts.extend(self.inline_parts(images));
                    ("user", parts)
                }
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
                        && !message.tool_calls.iter().any(|call| {
                            call.provider_meta
                                .as_deref()
                                .is_some_and(|sig| !sig.is_empty())
                        })
                    {
                        // The ordinal only: a call id and a tool name are
                        // provider-controlled text and this message is
                        // persisted in the session log.
                        return Err(anyhow!(
                            "Antigravity tool call 1 of {} in the pending batch carries no thoughtSignature, so this history cannot be replayed on this wire; start a new session or switch back to the model that produced it",
                            message.tool_calls.len()
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
                    images,
                    ..
                } => {
                    let mut response = json!({
                        "name": name,
                        "id": call_id,
                        "response": {"output": if *is_error {
                            format!("ERROR: {content}")
                        } else {
                            content.clone()
                        }},
                    });
                    // A result's images belong INSIDE its functionResponse, as
                    // its own `parts` — a reference client documents that
                    // placement as load-bearing, so they are never siblings.
                    if !images.is_empty() {
                        response["parts"] = json!(self.inline_parts(images));
                    }
                    ("user", vec![json!({"functionResponse": response})])
                }
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
        let session_id = derive_session_id(&contents);
        let mut request =
            json!({"contents": contents, "generationConfig": {}, "sessionId": session_id});
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
        repair_request(&mut request);
        // `requestType` marks an agent turn and `request.sessionId` identifies
        // the CONVERSATION (see `derive_session_id`). `requestId` is stamped
        // PER REQUEST by `complete`, not here: its step counts the requests of
        // one turn, and the retries are made there.
        Ok(json!({
            "project": PROJECT,
            "model": self.model,
            "requestType": REQUEST_TYPE,
            "userAgent": REQUEST_USER_AGENT,
            "request": request,
        }))
    }

    async fn send(&self, token: &str, request: &Value) -> anyhow::Result<reqwest::Response> {
        self.pace().await;
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

    /// Profile `vision` (default TRUE): images ride as `inlineData` parts (see
    /// `request_body`), so this adapter is vision-capable unless a profile says
    /// `vision = false`.
    fn supports_vision(&self) -> bool {
        self.vision
    }

    async fn complete(
        &mut self,
        messages: &[Message],
        tools: &[ToolSpec],
        on_delta: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        crate::agent::ensure_vision_supported(&self.model, self.vision, messages)?;
        let mut request = self.request_body(messages, tools)?;
        // The ids a `requestId` is built from, and the STEP counter: every
        // request of THIS turn advances the step (the 401 retry and the 404
        // fallback included), and the next turn starts again at 0. The counter
        // is a local because every retry of a turn is made here — no state has
        // to be threaded through a call path that lacks it.
        let (conversation, trajectory) = self.turn_ids(&request);
        let mut step = 0;
        set_request_id(&mut request, &conversation, &trajectory, step);
        let token = self.access_token().await?;
        let mut response = self.send(&token, &request).await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            // One bounded recovery: refresh once, then retry the same body.
            let token = self.refresh().await?;
            step += 1;
            set_request_id(&mut request, &conversation, &trajectory, step);
            response = self.send(&token, &request).await?;
        }
        if response.status() == reqwest::StatusCode::NOT_FOUND
            && let Some(tiered) = tiered_fallback(&self.model)
        {
            // One bounded fallback, never a loop: this client version is only
            // allowed to reach the shared `-tiered` endpoint for the family,
            // so the same turn is re-sent once with that model. The NEXT turn
            // starts from the configured id again (a 404 costs one request).
            let mut retried = request.clone();
            retried["model"] = json!(tiered);
            step += 1;
            set_request_id(&mut retried, &conversation, &trajectory, step);
            response = self.send(&token, &retried).await?;
            // A 429 on the fallback is the same 429: the retry below re-sends
            // THIS body (the tiered model), not the configured one.
            request = retried;
        }
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            // The body is classified and then DROPPED (see `Rate429`): only an
            // allowlisted reason and a parsed delay reach the message.
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let wait = match classify_429(status, body.as_bytes()) {
                Rate429::RateLimited { delay: Some(wait) } if wait <= RATE_LIMIT_429_CAP => wait,
                Rate429::Soft => SOFT_429_BACKOFF,
                Rate429::Unknown => UNKNOWN_429_BACKOFF,
                rate => return Err(rate_429_error(&self.model, rate)),
            };
            // Sleeping here does not bypass pacing: `send` still waits out
            // `min_interval`, so the retry is spaced like any other request.
            tokio::time::sleep(wait).await;
            step += 1;
            set_request_id(&mut request, &conversation, &trajectory, step);
            response = self.send(&token, &request).await?;
            // One retry is the whole budget: a second 429 fails on its status.
            if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Err(rate_429_error(&self.model, Rate429::Unknown));
            }
        }
        let status = response.status();
        // A redirect is a status-only failure here too: the body names another
        // URL, and this request carries the whole conversation.
        if status.is_redirection() {
            bail!(
                "Antigravity redirected the request (HTTP {status}, model={}); redirects are not followed",
                self.model
            );
        }
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
        let mut scanner = SseStream::default();
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

/// Bucket-id families whose ids split as `<family>-<window>` (`3p` is the
/// Claude/GPT family); any other family keeps its raw id.
const BUCKET_FAMILIES: &[&str] = &["gemini", "3p", "argon"];

/// One bucket of [`quota_summary`]: the two halves of its id (`gemini-5h` ->
/// `gemini` + `5h`), or the raw id alone when the family is unknown.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct QuotaBucket {
    pub family: String,
    /// `5h` / `weekly`; empty when the id was not split.
    pub window: String,
    /// `remainingFraction` (0..=1), when the response carries a number.
    pub remaining: Option<f64>,
    /// `resetTime`, verbatim.
    pub resets_at: Option<String>,
}

impl QuotaBucket {
    /// `gemini weekly`, or the raw id: an unknown bucket is shown, never lost.
    pub(crate) fn label(&self) -> String {
        if self.window.is_empty() {
            self.family.clone()
        } else {
            format!("{} {}", self.family, self.window)
        }
    }
}

/// The account's remaining quota, one row per bucket: one fresh read per call,
/// nothing cached or polled, and never part of a turn. With no
/// `client_id`/`client_secret` here it cannot refresh, so an expired login is
/// an error and the next turn refreshes it as usual.
pub(crate) async fn quota_summary() -> anyhow::Result<Vec<QuotaBucket>> {
    let mut model = AntigravityModel::new(String::new(), None, None, None, None)?;
    let token = model.access_token().await?;
    quota_summary_at(&model.client, QUOTA_URL, &token).await
}

/// The one POST behind [`quota_summary`], split out so a fake server can drive
/// it with a fixed URL and token. Status only on failure: no upstream body
/// reaches an error.
pub(crate) async fn quota_summary_at(
    client: &reqwest::Client,
    url: &str,
    token: &str,
) -> anyhow::Result<Vec<QuotaBucket>> {
    let response = client
        .post(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .bearer_auth(token)
        .json(&json!({"project": PROJECT}))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .context("Antigravity quota request failed")?;
    let value = response
        .json()
        .await
        .context("cannot decode the Antigravity quota response")?;
    parse_quota_summary(&value)
}

/// `{"groups":[{"buckets":[{"bucketId":"gemini-5h","remainingFraction":0.42,
/// "resetTime":"2026-11-27T00:00:00Z","disabled":false}]}]}`; a bucket with no
/// readable fraction keeps its row, and a body with no bucket is an error.
fn parse_quota_summary(value: &Value) -> anyhow::Result<Vec<QuotaBucket>> {
    let groups = value
        .get("groups")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("the Antigravity quota response has no groups"))?;
    let mut buckets = Vec::new();
    for bucket in groups
        .iter()
        .flat_map(|group| group.get("buckets").and_then(Value::as_array))
        .flatten()
    {
        let Some(id) = bucket.get("bucketId").and_then(Value::as_str) else {
            continue;
        };
        let (family, window) = match id.split_once('-') {
            Some((family, window)) if !window.is_empty() && BUCKET_FAMILIES.contains(&family) => {
                (family.to_owned(), window.to_owned())
            }
            _ => (id.to_owned(), String::new()),
        };
        buckets.push(QuotaBucket {
            family,
            window,
            remaining: bucket.get("remainingFraction").and_then(Value::as_f64),
            resets_at: bucket
                .get("resetTime")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    if buckets.is_empty() {
        bail!("the Antigravity quota response has no bucket");
    }
    Ok(buckets)
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

/// The `finishReason` values this wire is known to report. A value is matched
/// against these EXACT constants and the constant is what gets printed — never
/// the body's own text, exactly like [`error_code`]: these messages are
/// persisted in the session log.
const FINISH_REASONS: &[&str] = &[
    // Terminal: the candidate stopped on its own.
    "STOP",
    // Terminal: the output limit was reached (the answer/calls are truncated).
    "MAX_TOKENS",
    // Terminal failures: no part of the turn is a usable answer.
    "MALFORMED_FUNCTION_CALL",
    "SAFETY",
    "RECITATION",
    "BLOCKLIST",
    "PROHIBITED_CONTENT",
    "SPII",
    "IMAGE_SAFETY",
    "UNEXPECTED_TOOL_CALL",
    "LANGUAGE",
    "OTHER",
    "FINISH_REASON_UNSPECIFIED",
];

/// The [`FINISH_REASONS`] constant equal to `candidate`, if any. An unknown
/// value yields `None` and is never echoed; it is not evidence of success.
fn allowed_finish(candidate: &str) -> Option<&'static str> {
    FINISH_REASONS
        .iter()
        .copied()
        .find(|reason| *reason == candidate)
}

/// The accumulated turn plus the wire quirks worth naming: usage settles on
/// the LAST element (earlier ones repeat partial counts), only the terminal
/// `finishReason` decides whether the turn may be used at all, and a
/// `thoughtSignature` may be its own part immediately after its call — even in
/// the NEXT element.
#[derive(Default)]
struct Turn {
    content: String,
    tool_calls: Vec<ToolCall>,
    usage: Option<UsageMetadata>,
    /// The last `finishReason` any candidate reported: untrusted text, vetted
    /// by [`allowed_finish`] before it is printed or acted on. Recorded apart
    /// from [`Turn::usage`] on purpose — a turn can carry usage and still have
    /// terminated badly (or not at all).
    finish_reason: Option<String>,
    /// Index of the call a signature-only part may still sign. An intervening
    /// part clears it; it SURVIVES into the following element, because the wire
    /// splits a call and its signature part across elements.
    pending_call: Option<usize>,
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
            // itself, or a signature-only part sitting DIRECTLY after its call.
            // Nothing is copied across parts, and any intervening part breaks
            // the association — but the association outlives the element, so a
            // signature part in the next chunk still signs the call it follows.
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
                        // The ordinal and a short digest, never the id itself:
                        // provider-controlled text is not echoed into a
                        // message the runner persists.
                        bail!(
                            "Antigravity stream repeated a tool call id (digest {}) at call {} of this turn, so two results would route to one call",
                            short_digest(&id),
                            self.tool_calls.len() + 1
                        );
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
                    self.pending_call = Some(self.tool_calls.len() - 1);
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
                    self.pending_call = None;
                    continue;
                }
                // Signature-only: no text AT ALL (absent or empty) and no
                // call. This is the shape the wire uses when the signature is
                // its own part rather than a field of the call.
                if let Some(signature) = part["thoughtSignature"].as_str() {
                    if let Some(call) = self
                        .pending_call
                        .take()
                        .and_then(|index| self.tool_calls.get_mut(index))
                    {
                        call.provider_meta
                            .get_or_insert_with(|| signature.to_owned());
                    }
                    continue;
                }
                // Any other part — an empty text, an unknown field —
                // INTERVENES: a signature part after it no longer sits
                // directly after its call.
                self.pending_call = None;
            }
        }
        Ok(())
    }

    /// Validate, then assemble: the whole selected candidate is finished BEFORE
    /// anything is returned, so a batch that could never be replayed is refused
    /// before the caller runs a single call from it.
    fn finish(self) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        let calls = self.tool_calls.len();
        match self.finish_reason.as_deref().map(allowed_finish) {
            // A TEXT-only turn may end without a `finishReason`: the live stream
            // does not always send one (verified — a complete "pong" answer
            // carried usage and a signature part but no finishReason), text has
            // no side effect, and transport truncation is caught by the framer's
            // EOF check. A CALL batch has no such excuse: without a terminal
            // reason nothing proves the batch was complete.
            None if calls == 0 => {}
            None => {
                bail!("Antigravity stream ended without a finishReason, so this turn is incomplete")
            }
            // An unrecognized value is not evidence of success, and the value
            // is provider-controlled text: it is never echoed.
            Some(None) => bail!(
                "Antigravity stream finished with an unrecognized finishReason (incomplete turn)"
            ),
            // The one verified success: the candidate stopped on its own.
            Some(Some("STOP")) => {}
            // Length-limited TEXT is still displayable: a truncated answer is
            // shown as the partial answer it is, with the reason in the usage.
            // A length-limited CALL BATCH is not: its arguments can be cut
            // mid-call, so none of it runs.
            Some(Some("MAX_TOKENS")) if calls == 0 => {}
            Some(Some("MAX_TOKENS")) => bail!(
                "Antigravity stream stopped at its token limit while asking for {calls} tool call(s); they were not run"
            ),
            // MALFORMED_FUNCTION_CALL, SAFETY, RECITATION, ... : the parts are
            // fragments of a generation the provider itself rejected.
            Some(Some(failure)) => {
                bail!("Antigravity stream finished with {failure}, not a completed turn")
            }
        }
        let finish = self.finish_reason.as_deref().and_then(allowed_finish);
        self.assemble(finish)
    }

    /// The shared tail: an empty turn is not a turn, a batch whose first call
    /// carries no signature cannot be replayed, and the usage/finish mapping is
    /// the same whichever transport delivered it.
    fn assemble(
        self,
        finish: Option<&'static str>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        if self.content.is_empty() && self.tool_calls.is_empty() {
            bail!("Antigravity stream ended without content or tool calls (incomplete turn)");
        }
        // Live evidence: the FIRST call of a batch carries the signature and
        // its siblings stay unsigned. A batch whose first call has none can
        // never be replayed (400 INVALID_ARGUMENT), so it is rejected HERE —
        // a signature is never borrowed from a text part or another candidate.
        if self
            .tool_calls
            .first()
            .is_some_and(|call| call.provider_meta.as_deref().unwrap_or_default().is_empty())
        {
            bail!(
                "Antigravity returned {} tool call(s) but the first one carries no thoughtSignature, so this batch cannot be replayed on this wire; they were not run",
                self.tool_calls.len()
            );
        }
        let usage = self.usage.map(|metadata| Usage {
            input_tokens: metadata.prompt,
            // Thinking tokens are generated output the caller pays for.
            output_tokens: metadata.candidates + metadata.thoughts.unwrap_or(0),
            cache_hit_tokens: None,
            cache_miss_tokens: None,
            reasoning_tokens: metadata.thoughts,
            // The vetted constant, never the body's own text.
            finish_reason: finish.map(str::to_owned),
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

/// Incremental reader for the `?alt=sse` stream: `data: {json}` lines, one
/// blank line between events, CRLF or LF (the service sends CRLF, verified
/// live). A multi-line `data:` field is joined with `\n` per the SSE spec, so a
/// JSON object split across lines still decodes. Only `data:` counts; other
/// fields (`event:`, `id:`, `retry:`, `:` comments) are ignored, and an empty
/// payload (a keep-alive) is skipped.
///
/// The truncation rule is the security-relevant half: the service also closes
/// an ABORTED stream, so an EOF with a half-read line or an undispatched event
/// is an error, never a shorter successful turn — a truncated turn's tool calls
/// must not run.
#[derive(Default)]
struct SseStream {
    /// Bytes of a line still waiting for its terminator.
    pending: Vec<u8>,
    /// Accumulated `data:` payload of the event being read.
    data: Option<String>,
    /// A terminator (`[DONE]`) was seen: everything after it is ignored.
    done: bool,
}

/// The SSE terminator some proxies append. This wire ends its last event with
/// a blank line and sends no terminator (verified live); accepting one costs a
/// comparison and keeps a proxy in front of it from breaking the turn.
const SSE_DONE: &str = "[DONE]";

impl SseStream {
    fn push(&mut self, chunk: &[u8]) -> anyhow::Result<Vec<Value>> {
        self.pending.extend_from_slice(chunk);
        let mut events = Vec::new();
        let mut consumed = 0;
        while let Some(offset) = self.pending[consumed..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let line_end = consumed + offset;
            let mut line = &self.pending[consumed..line_end];
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            let line = String::from_utf8_lossy(line).into_owned();
            self.line(&line, &mut events)?;
            consumed = line_end + 1;
        }
        self.pending.drain(..consumed);
        Ok(events)
    }

    /// One complete line: a blank line dispatches the event, `data:` accumulates
    /// into it, anything else is not payload.
    fn line(&mut self, line: &str, events: &mut Vec<Value>) -> anyhow::Result<()> {
        if line.is_empty() {
            return self.dispatch(events);
        }
        let Some(payload) = line.strip_prefix("data:") else {
            return Ok(());
        };
        let payload = payload.strip_prefix(' ').unwrap_or(payload);
        let data = self.data.get_or_insert_with(String::new);
        if !data.is_empty() {
            data.push('\n');
        }
        data.push_str(payload);
        Ok(())
    }

    /// A blank line ends the event. An empty payload, anything after a
    /// terminator, and the terminator itself yield no element.
    fn dispatch(&mut self, events: &mut Vec<Value>) -> anyhow::Result<()> {
        let Some(data) = self.data.take() else {
            return Ok(());
        };
        if self.done || data.trim().is_empty() {
            return Ok(());
        }
        if data.trim() == SSE_DONE {
            self.done = true;
            return Ok(());
        }
        events.push(
            serde_json::from_str(&data).context("cannot decode an Antigravity stream event")?,
        );
        Ok(())
    }

    /// Called once at a clean EOF: no line may be half-read and no event may be
    /// waiting for its blank line.
    fn finish(&mut self) -> anyhow::Result<()> {
        if self.data.is_some() || !self.pending.is_empty() {
            bail!("Antigravity response stream ended mid-event (truncated turn)");
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

/// Pre-send REPAIR, in the spirit of the mature implementation of this wire:
/// a repairable request is sent instead of refused. Empty parts are dropped
/// (and a content left with no parts at all goes with them), a missing
/// `functionCall` id is filled and the matching `functionResponse` reuses it, a
/// `functionResponse` whose `name` drifted from its `functionCall` is aligned,
/// and a tool declaration without a schema gets an object schema. Signatures
/// are NEVER invented: the unsigned-first-call refusal already happened in
/// [`AntigravityModel::request_body`], which is the one 400 we verified live.
fn repair_request(request: &mut Value) {
    let Some(contents) = request["contents"].as_array_mut() else {
        return;
    };
    // Calls first: the id filled here is the one the matching result must use.
    let mut call_names: std::collections::HashMap<String, String> = Default::default();
    let mut unanswered: Vec<String> = Vec::new();
    for content in contents.iter_mut() {
        let Some(parts) = content["parts"].as_array_mut() else {
            continue;
        };
        parts.retain(|part| !is_empty_part(part));
        for part in parts.iter_mut() {
            let Some(call) = part.get_mut("functionCall") else {
                continue;
            };
            if call["id"].as_str().is_none_or(str::is_empty) {
                call["id"] = json!(local_call_id());
            }
            let id = call["id"].as_str().unwrap_or_default().to_owned();
            if let Some(name) = call["name"].as_str() {
                call_names.insert(id.clone(), name.to_owned());
            }
            unanswered.push(id);
        }
    }
    for content in contents.iter_mut() {
        let Some(parts) = content["parts"].as_array_mut() else {
            continue;
        };
        for part in parts.iter_mut() {
            let Some(response) = part.get_mut("functionResponse") else {
                continue;
            };
            let mut id = response["id"].as_str().unwrap_or_default().to_owned();
            if id.is_empty() {
                // The next call still waiting for its result.
                id = unanswered.first().cloned().unwrap_or_else(local_call_id);
                response["id"] = json!(id);
            }
            if let Some(position) = unanswered.iter().position(|open| *open == id) {
                unanswered.remove(position);
            }
            if let Some(name) = call_names.get(&id) {
                response["name"] = json!(name);
            }
        }
    }
    contents.retain(|content| {
        content["parts"]
            .as_array()
            .is_some_and(|parts| !parts.is_empty())
    });
    // `pointer_mut`, not `request["tools"][0]`: a request with no tools has a
    // JSON null there, and indexing a null panics.
    for declaration in request
        .pointer_mut("/tools/0/functionDeclarations")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        if declaration["parametersJsonSchema"].is_null() {
            declaration["parametersJsonSchema"] = json!({"type": "object"});
        }
    }
}

/// A part with nothing the wire can use: no call, no response, no nonempty
/// text, no signature. A signature-only part is kept — it belongs to the call
/// beside it and dropping it would make the batch unreplayable.
fn is_empty_part(part: &Value) -> bool {
    if part.get("functionCall").is_some()
        || part.get("functionResponse").is_some()
        || part.get("inlineData").is_some()
    {
        return false;
    }
    !part["text"].as_str().is_some_and(|text| !text.is_empty())
        && !part["thoughtSignature"]
            .as_str()
            .is_some_and(|signature| !signature.is_empty())
}

/// A unique, persisted id for a call the wire did not identify itself: pending
/// questions are keyed by call id, so a per-turn counter would repeat across
/// turns.
fn local_call_id() -> String {
    format!("e-agent-call-{:016x}", rand::random::<u64>())
}

/// The `requestId` the official client sends:
/// `agent/<conversation>/<millis>/<trajectory>/<step>` — the shape 9router
/// validates as `/^agent\/[^/]+\/\d+\/[^/]+\/\d+$/` and its hardening
/// notes name as a cause of 403s, so a plain opaque token cannot stand in.
fn request_id(conversation: &str, millis: i64, trajectory: &str, step: u64) -> String {
    format!("agent/{conversation}/{millis}/{trajectory}/{step}")
}

/// Stamp the next request's id: the current time is read HERE, so a retry
/// carries its own `<millis>` with the same conversation and trajectory.
fn set_request_id(request: &mut Value, conversation: &str, trajectory: &str, step: u64) {
    request["requestId"] = json!(request_id(
        conversation,
        Utc::now().timestamp_millis(),
        trajectory,
        step
    ));
}

/// SHA-256 over the role and the serialized parts of every content, first 16
/// bytes: the shared input of `sessionId`, `<conversation>`, and
/// `<trajectory>`. Deterministic on purpose — the same transcript always yields
/// the same digest, while a compaction, a fork, or any added turn changes it.
/// Taken BEFORE repair fills in generated ids, so an unchanged transcript cannot
/// produce a new one.
fn conversation_digest(contents: &[Value]) -> [u8; 16] {
    let mut hasher = Sha256::new();
    for content in contents {
        hasher.update(content["role"].as_str().unwrap_or_default().as_bytes());
        hasher.update([0]);
        for part in content["parts"].as_array().into_iter().flatten() {
            hasher.update(part.to_string().as_bytes());
            hasher.update([0]);
        }
    }
    let digest = Sha256::digest(hasher.finalize());
    let mut truncated = [0u8; 16];
    truncated.copy_from_slice(&digest[..16]);
    truncated
}

/// `request.sessionId`: the digest as 32 hex characters. Its shape is not
/// load-bearing (only `requestId`'s is), so it stays exactly as it was.
fn derive_session_id(contents: &[Value]) -> String {
    conversation_digest(contents)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A UUID-shaped (version 4, RFC 4122 variant) rendering of a digest: 8-4-4-4-12
/// hex, so `requestId`'s segments look like the client's UUIDs without pulling
/// in a uuid crate.
fn uuid_like(mut digest: [u8; 16]) -> String {
    digest[6] = (digest[6] & 0x0f) | 0x40;
    digest[8] = (digest[8] & 0x3f) | 0x80;
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// The `-tiered` retry target for `model`, if this backend is known to serve
/// the family that way: a pinned client version (our `agy` 1.5.14 too) is
/// refused for the bare and per-tier flash ids with 404 while the SHARED tier
/// endpoint answers 200. Only Gemini **flash** families take part, an id that
/// already ends in `-tiered` has no second chance, and the caller retries ONCE.
fn tiered_fallback(model: &str) -> Option<String> {
    if model.ends_with(TIERED_SUFFIX) {
        return None;
    }
    let base = ["-high", "-medium", "-low"]
        .iter()
        .find_map(|tier| model.strip_suffix(tier))
        .unwrap_or(model);
    base.strip_prefix("gemini-")
        .filter(|family| family.ends_with("-flash"))?;
    Some(format!("{base}{TIERED_SUFFIX}"))
}

/// A short digest standing in for a provider-controlled identifier (a call id,
/// a tool name) in an error message. Those messages are persisted in the
/// session log and such a value can be token-shaped, so the value itself is
/// never repeated: the digest is enough to correlate two lines, and not enough
/// to quote one.
fn short_digest(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    format!(
        "{:08x}",
        u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]])
    )
}

/// What a 429 means on this wire: one status, several meanings that need
/// different reactions. Only an ALLOWLISTED reason constant and a PARSED delay
/// reach a message — never the body, which can quote the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rate429 {
    /// `QUOTA_EXHAUSTED`: the (weekly) window is spent; retrying cannot help.
    QuotaExhausted { reset: Option<Duration> },
    /// `RATE_LIMIT_EXCEEDED`: too fast right now, with an optional hint.
    RateLimited { delay: Option<Duration> },
    /// "try again" / "temporarily": one short retry is expected to work.
    Soft,
    /// Nothing this adapter knows: one short retry, then the status alone.
    Unknown,
}

/// Body markers, lowercase. Quota first: a body naming a spent quota AND "try
/// again" is the condition a retry cannot fix. The `ErrorInfo.reason` values
/// (`QUOTA_EXHAUSTED`, `RATE_LIMIT_EXCEEDED`) lowercase into these tables.
#[rustfmt::skip]
const QUOTA_MARKERS: &[&str] = &[
    "quota_exhausted", "quota reached", "enable overages", "individual quota",
    "insufficient credit", "free tier", "daily limit", "exhausted your capacity",
];
#[rustfmt::skip]
const RATE_MARKERS: &[&str] = &[
    "rate_limit_exceeded", "per minute", "rpm", "rate limit", "too many requests",
    "reset in 0s", "reset after 0s",
];
const SOFT_MARKERS: &[&str] = &["try again", "temporarily"];
/// A `rate_limited` hint above this is not slept: the turn fails with it instead.
const RATE_LIMIT_429_CAP: Duration = Duration::from_secs(60);
const SOFT_429_BACKOFF: Duration = Duration::from_secs(2);
const UNKNOWN_429_BACKOFF: Duration = Duration::from_secs(5);

/// Classify a 429 body; a non-429 is not this decision's business.
fn classify_429(status: reqwest::StatusCode, body: &[u8]) -> Rate429 {
    if status != reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Rate429::Unknown;
    }
    // `ErrorInfo.reason` is text like `QUOTA_EXHAUSTED`, so the marker tables
    // below cover it (`quota_exhausted`, `rate_limit_exceeded`); the JSON body
    // is only needed for the delay.
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    let value: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let value = value
        .as_array()
        .and_then(|array| array.first())
        .unwrap_or(&value);
    let hint = delay_hint(&text, value);
    let marker = |set: &[&str]| set.iter().any(|marker| text.contains(marker));
    if marker(QUOTA_MARKERS) {
        Rate429::QuotaExhausted { reset: hint }
    } else if marker(RATE_MARKERS) {
        Rate429::RateLimited { delay: hint }
    } else if marker(SOFT_MARKERS) {
        Rate429::Soft
    } else {
        Rate429::Unknown
    }
}

/// The delay a 429 body carries: `RetryInfo.retryDelay` (protobuf JSON
/// duration) anywhere in the document, else a `reset in Ns|Nm|Nh` phrase.
fn delay_hint(text: &str, value: &Value) -> Option<Duration> {
    fn walk(value: &Value) -> Option<&str> {
        match value {
            Value::Array(items) => items.iter().find_map(walk),
            Value::Object(map) => map
                .get("retryDelay")
                .and_then(Value::as_str)
                .or_else(|| map.values().find_map(walk)),
            _ => None,
        }
    }
    if let Some(seconds) = walk(value).and_then(parse_seconds) {
        return Some(Duration::from_secs_f64(seconds));
    }
    let tail = ["reset in ", "reset after "]
        .iter()
        .find_map(|prefix| text.split(prefix).nth(1))?
        .trim_start();
    let number = tail
        .split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .next()?;
    let unit = tail[number.len()..]
        .trim_start()
        .chars()
        .next()
        .unwrap_or('s');
    parse_seconds(&format!("{number}{unit}")).map(Duration::from_secs_f64)
}

/// `"12s"` / `"1.500s"` / `"5m"` / `"2h"` / `"12"` -> seconds, by hand: the
/// forms are fixed and a crate would be more machinery than this.
fn parse_seconds(raw: &str) -> Option<f64> {
    let raw = raw.trim();
    let (number, factor) = match raw.chars().last()? {
        's' => (&raw[..raw.len() - 1], 1.0),
        'm' => (&raw[..raw.len() - 1], 60.0),
        'h' => (&raw[..raw.len() - 1], 3600.0),
        _ => (raw, 1.0),
    };
    let seconds: f64 = number.trim().parse().ok()?;
    (seconds.is_finite() && seconds >= 0.0).then_some(seconds * factor)
}

/// The one-line, actionable message for a 429 the adapter will not retry: only
/// allowlisted reason constants and parsed delays are interpolated.
fn rate_429_error(model: &str, rate: Rate429) -> anyhow::Error {
    let until = |wait: Duration| {
        let reset =
            Utc::now() + ChronoDuration::from_std(wait).unwrap_or_else(|_| ChronoDuration::zero());
        reset.format("%Y-%m-%dT%H:%M:%SZ").to_string()
    };
    match rate {
        Rate429::QuotaExhausted { reset } => anyhow!(
            "Antigravity quota exhausted (HTTP 429 QUOTA_EXHAUSTED, model={model}){}; retrying cannot help",
            reset
                .map(|wait| format!(", resets in {}s (until {})", wait.as_secs(), until(wait)))
                .unwrap_or_else(|| " with no reset time given".to_owned())
        ),
        Rate429::RateLimited { delay } => anyhow!(
            "Antigravity rate limited (HTTP 429 RATE_LIMIT_EXCEEDED, model={model}){}; try again later",
            delay
                .map(|wait| format!(
                    ", retry in {}s (the cap is {}s)",
                    wait.as_secs(),
                    RATE_LIMIT_429_CAP.as_secs()
                ))
                .unwrap_or_else(|| " with no retry delay given".to_owned())
        ),
        Rate429::Soft | Rate429::Unknown => {
            anyhow!("Antigravity returned HTTP 429 (model={model})")
        }
    }
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
    // Reasons this wire is known to name (the 429 split lives in `Rate429`).
    "API_KEY_INVALID",
    "API_KEY_EXPIRED",
    "QUOTA_EXHAUSTED",
    "RATE_LIMIT_EXCEEDED",
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
        .chain(
            error["details"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|detail| detail["reason"].as_str()),
        )
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
