use anyhow::{Context, anyhow, bail};
use base64::Engine;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::agent::{
    AssistantMessage, Message, Model, ModelDeltaKind, ToolCall, ToolSpec, Usage, preview,
};
use crate::codex_auth::CodexAuth;

const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

/// Shared transient retry policy for model requests (both the codex wire here
/// and the chat wire in src/model.rs). Three independent layers, matching how
/// far the request got before failing:
///
/// 1. **Connect/timeout** (`CONNECT_RETRY_ATTEMPTS`, 8 attempts, exponential
///    backoff `base * 2^(attempt-1)` — in production 0.5/1/2/4/8/16/32s, a
///    ~63.5s recovery window): only errors where no bytes were exchanged
///    qualify (`is_timeout() || is_connect() || is_request()` — the latter
///    covers TLS-layer send failures such as rustls decrypt errors). Peak-hour
///    network jitter lasting tens of seconds self-heals before a turn fails.
/// 2. **HTTP 403/5xx** (`HTTP_RETRY_ATTEMPTS`, 3 whole-request retries,
///    backoff 0.8/1.6/3.2s): 403 (rate limit) and 5xx (server overload) are
///    transient server-side conditions. The request is an idempotent
///    generation request (no side effects), so re-sending is safe. Other 4xx
///    client errors and 401 auth errors are never retried — retrying a
///    rejected request cannot fix a client bug.
/// 3. **SSE stream decode failures** (same `HTTP_RETRY_ATTEMPTS` budget, see
///    `is_stream_decode_error`): a stream cut short mid-flight — an
///    eventsource parse/transport error (e.g. reqwest's "error decoding
///    response body" when the body stream dies) or a data frame that is not
///    valid JSON ("cannot decode ... event") — restarts the whole idempotent
///    request. Well-formed provider-level terminal events (response.failed /
///    response.incomplete / response.error, chat stream error chunks) are NOT
///    retried.
pub(crate) const CONNECT_RETRY_ATTEMPTS: u32 = 8;
#[cfg(not(test))]
pub(crate) const CONNECT_RETRY_BASE_BACKOFF_MS: u64 = 500;
/// Tests scale the base down (10ms) so exhaustive-retry tests stay fast while
/// still exercising the attempt count and the exponential doubling schedule.
#[cfg(test)]
pub(crate) const CONNECT_RETRY_BASE_BACKOFF_MS: u64 = 10;

/// Whole-request retry budget shared by layers 2 and 3 above (HTTP 403/5xx
/// responses and SSE streams that die mid-flight): 3 attempts with exponential
/// backoff `base * 2^(attempt-1)` — in production 0.8/1.6/3.2s (~5.6s window).
/// Shared with the chat wire in src/model.rs.
pub(crate) const HTTP_RETRY_ATTEMPTS: u32 = 3;
#[cfg(not(test))]
pub(crate) const HTTP_RETRY_BASE_BACKOFF_MS: u64 = 800;
#[cfg(test)]
pub(crate) const HTTP_RETRY_BASE_BACKOFF_MS: u64 = 10;

/// Streaming delta sink shared by both wires' stream consumers
/// (`consume_stream` in this file and in src/model.rs).
pub(crate) type DeltaSink<'a> = Option<&'a mut (dyn for<'b> FnMut(ModelDeltaKind, &'b str) + Send)>;

/// Exponential backoff before retry `attempt` (1-based): `base * 2^(attempt-1)`.
pub(crate) fn retry_backoff_ms(base_ms: u64, attempt: u32) -> u64 {
    base_ms * 2u64.pow(attempt - 1)
}

/// Transient server-side HTTP statuses worth retrying: 403 (rate limit) and
/// any 5xx (overload). Other 4xx client errors and 401 auth errors are not
/// retried.
pub(crate) fn is_retryable_http_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::FORBIDDEN || status.is_server_error()
}

/// True when a stream error means the SSE response was cut short mid-flight:
/// an eventsource parse/transport failure (the transport variant wraps the
/// reqwest body error — e.g. "error decoding response body" when the
/// connection dies mid-stream) or a data frame that is not valid JSON. The
/// request never completed, so re-running the whole idempotent request is
/// safe. Well-formed provider-level errors (response.failed /
/// response.incomplete / response.error, chat stream error chunks, incomplete
/// tool calls) are NOT classified as decode errors and are not retried.
pub(crate) fn is_stream_decode_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.is::<eventsource_stream::EventStreamError<reqwest::Error>>()
            || cause.is::<serde_json::Error>()
    })
}

#[derive(Clone)]
pub struct CodexModel {
    client: reqwest::Client,
    auth: CodexAuth,
    model: String,
    reasoning_effort: Option<String>,
    endpoint: String,
    /// Whether the model accepts image input (responses wire builds
    /// `input_image` parts only for vision-capable models).
    vision: bool,
    /// Global content-addressed image store (see `agent::image_store_dir`).
    image_store: Option<PathBuf>,
}

impl CodexModel {
    pub fn new(
        auth: CodexAuth,
        model: String,
        reasoning_effort: Option<String>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(600))
                .build()?,
            auth,
            model,
            reasoning_effort,
            endpoint: RESPONSES_URL.into(),
            vision: false,
            image_store: crate::agent::image_store_dir(),
        })
    }

    /// Mark the model as vision-capable so user messages with attached
    /// images pass the vision gate.
    pub fn with_vision(mut self, vision: bool) -> Self {
        self.vision = vision;
        self
    }

    #[cfg(test)]
    fn with_endpoint(auth: CodexAuth, endpoint: String) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(2))
                .build()
                .unwrap(),
            auth,
            model: "codex".into(),
            reasoning_effort: Some("high".into()),
            endpoint,
            vision: false,
            image_store: crate::agent::image_store_dir(),
        }
    }

    async fn send(
        &self,
        request: &ResponsesRequest<'_>,
        refresh: bool,
    ) -> anyhow::Result<(reqwest::Response, String)> {
        let (access_token, account_id) = if refresh {
            self.auth.access_token_and_account().await?
        } else {
            self.auth.current_access_token_and_account().await?
        };
        // Transient connectivity hiccups (e.g. "tls handshake eof") recover on
        // retry; only connection/timeout errors qualify, HTTP status errors are
        // handled by the caller (401 refresh, then the HTTP 403/5xx retry loop
        // in complete()). 8 attempts with exponential backoff
        // (0.5/1/2/4/8/16/32s, ~63.5s window) so network jitter lasting tens of
        // seconds self-heals; shared with src/model.rs's chat wire.
        let mut last_error = None;
        for attempt in 1..=CONNECT_RETRY_ATTEMPTS {
            match self
                .client
                .post(&self.endpoint)
                .bearer_auth(&access_token)
                .header("ChatGPT-Account-ID", account_id.as_str())
                .header("originator", "codex_cli_rs")
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .header(reqwest::header::USER_AGENT, "codex_cli_rs/0.0.0 (e-agent)")
                .json(request)
                .send()
                .await
            {
                Ok(response) => return Ok((response, access_token)),
                Err(error)
                    if attempt < CONNECT_RETRY_ATTEMPTS
                        && (error.is_timeout() || error.is_connect() || error.is_request()) =>
                {
                    last_error = Some(error);
                    tokio::time::sleep(Duration::from_millis(retry_backoff_ms(
                        CONNECT_RETRY_BASE_BACKOFF_MS,
                        attempt,
                    )))
                    .await;
                }
                Err(error) => return Err(error).context("ChatGPT Responses request failed"),
            }
        }
        // Unreachable in practice (every arm above returns); defensive.
        Err(anyhow::anyhow!(
            "ChatGPT Responses request failed: {last_error:?}"
        ))
    }
}

#[async_trait::async_trait]
impl Model for CodexModel {
    fn name(&self) -> &str {
        &self.model
    }

    fn supports_vision(&self) -> bool {
        self.vision
    }

    async fn complete(
        &mut self,
        messages: &[Message],
        tools: &[ToolSpec],
        mut on_delta: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        crate::agent::ensure_vision_supported(&self.model, self.vision, messages)?;
        let request = ResponsesRequest::from_internal(
            &self.model,
            self.reasoning_effort.as_deref(),
            messages,
            tools,
            self.image_store.as_deref(),
        );
        // Whole-request retry loop: HTTP 403/5xx responses and SSE streams cut
        // short mid-flight both restart the idempotent request (see
        // is_retryable_http_status / is_stream_decode_error). Connect/timeout
        // errors never reach this loop — send() already exhausted
        // CONNECT_RETRY_ATTEMPTS and returned. Deltas streamed to on_delta
        // before a decode failure are discarded: only the last successful
        // attempt's accumulation is returned, and re-running an idempotent
        // generation request is safe.
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let (mut response, rejected_access_token) = self.send(&request, true).await?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED {
                self.auth
                    .refresh_after_unauthorized(&rejected_access_token)
                    .await?;
                response = self.send(&request, false).await?.0;
            }
            let status = response.status();
            if is_retryable_http_status(status) && attempt < HTTP_RETRY_ATTEMPTS {
                tokio::time::sleep(Duration::from_millis(retry_backoff_ms(
                    HTTP_RETRY_BASE_BACKOFF_MS,
                    attempt,
                )))
                .await;
                continue;
            }
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                bail!(
                    "ChatGPT Responses returned HTTP {status}: {}",
                    preview(&body, 500)
                );
            }
            match consume_stream(response, &mut on_delta).await {
                Ok(result) => return Ok(result),
                Err(error) if attempt < HTTP_RETRY_ATTEMPTS && is_stream_decode_error(&error) => {
                    tokio::time::sleep(Duration::from_millis(retry_backoff_ms(
                        HTTP_RETRY_BASE_BACKOFF_MS,
                        attempt,
                    )))
                    .await;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

/// Consume an SSE response stream, accumulating content / reasoning / tool
/// calls / usage into an assistant message. Returns Err for any stream
/// failure; the caller decides whether a decode failure is retryable
/// (`is_stream_decode_error`).
async fn consume_stream(
    response: reqwest::Response,
    on_delta: &mut DeltaSink<'_>,
) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut calls = Vec::new();
    let mut usage = None;
    let mut completed = false;
    let mut events = response.bytes_stream().eventsource();
    while let Some(event) = events.next().await {
        let event = event.context("cannot parse ChatGPT Responses event")?;
        if event.data == "[DONE]" {
            break;
        }
        let value: Value =
            serde_json::from_str(&event.data).context("cannot decode ChatGPT Responses event")?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(event.event.as_str());
        match kind {
            "response.output_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    content.push_str(delta);
                    if let Some(callback) = on_delta {
                        callback(ModelDeltaKind::Content, delta);
                    }
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    reasoning.push_str(delta);
                    if let Some(callback) = on_delta {
                        callback(ModelDeltaKind::Reasoning, delta);
                    }
                }
            }
            "response.reasoning_summary_part.added" if !reasoning.is_empty() => {
                reasoning.push_str("\n\n");
                if let Some(callback) = on_delta {
                    callback(ModelDeltaKind::Reasoning, "\n\n");
                }
            }
            "response.output_item.done" => {
                let item = value.get("item").unwrap_or(&value);
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
                    let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                    let arguments = item.get("arguments").and_then(Value::as_str).unwrap_or("");
                    if id.is_empty() || name.is_empty() {
                        bail!("ChatGPT Responses returned an incomplete function call");
                    }
                    calls.push(ToolCall {
                        id: id.into(),
                        name: name.into(),
                        arguments: arguments.into(),
                    });
                }
            }
            "response.completed" => {
                let response = value.get("response").unwrap_or(&value);
                usage = response.get("usage").map(response_usage).transpose()?;
                completed = true;
            }
            "response.failed" | "response.incomplete" | "error" | "response.error" => {
                bail!("ChatGPT Responses {kind}: {}", provider_error(&value))
            }
            _ => {}
        }
    }
    if !completed {
        bail!("ChatGPT Responses stream ended before response.completed");
    }
    Ok((
        AssistantMessage {
            content: (!content.is_empty()).then_some(content),
            tool_calls: calls,
            reasoning: (!reasoning.is_empty()).then_some(reasoning),
        },
        usage,
    ))
}

fn response_usage(value: &Value) -> anyhow::Result<Usage> {
    Ok(Usage {
        input_tokens: value
            .get("input_tokens")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("ChatGPT Responses completed without input token usage"))?,
        output_tokens: value
            .get("output_tokens")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("ChatGPT Responses completed without output token usage"))?,
    })
}

fn provider_error(value: &Value) -> String {
    value
        .get("error")
        .and_then(|v| v.get("message"))
        .or_else(|| value.get("message"))
        .and_then(Value::as_str)
        .map(|v| preview(v, 500))
        .unwrap_or_else(|| preview(&value.to_string(), 500))
}

#[derive(Serialize)]
struct ResponsesRequest<'a> {
    model: &'a str,
    instructions: String,
    input: Vec<Value>,
    tools: Vec<Value>,
    tool_choice: &'static str,
    parallel_tool_calls: bool,
    store: bool,
    stream: bool,
    include: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Value>,
}

impl<'a> ResponsesRequest<'a> {
    fn from_internal(
        model: &'a str,
        reasoning_effort: Option<&str>,
        messages: &[Message],
        tools: &[ToolSpec],
        image_store: Option<&Path>,
    ) -> Self {
        let instructions = messages
            .iter()
            .filter_map(|message| match message {
                Message::System { content } => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let input = messages.iter().filter_map(|message| match message {
            Message::System { .. } => None,
            // A user message with attached images appends `input_image` parts
            // (image_url is a BARE STRING here, unlike the chat wire's object
            // form). A missing store file degrades to a text placeholder part.
            Message::User { content, images } => {
                let mut parts = vec![json!({"type":"input_text","text":content})];
                for image in images {
                    match crate::agent::load_image_bytes(image_store, &image.hash) {
                        Some(bytes) => parts.push(json!({"type":"input_image","image_url":format!("data:{};base64,{}", image.mime, base64::engine::general_purpose::STANDARD.encode(bytes))})),
                        None => parts.push(json!({"type":"input_text","text":format!("[image missing: {}]", image.hash)})),
                    }
                }
                Some(json!({"type":"message","role":"user","content":parts}))
            }
            Message::Assistant(message) => {
                let mut items = Vec::new();
                if let Some(content) = &message.content { items.push(json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":content}]})); }
                items.extend(message.tool_calls.iter().map(|call| json!({"type":"function_call","name":call.name,"arguments":call.arguments,"call_id":call.id})));
                Some(Value::Array(items))
            }
            Message::Tool { call_id, content, is_error, .. } => Some(json!({"type":"function_call_output","call_id":call_id,"output":if *is_error { format!("ERROR: {content}") } else { content.clone() }})),
        }).flat_map(|value| match value { Value::Array(values) => values, value => vec![value] }).collect();
        let tools = tools.iter().map(|tool| json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.parameters,"strict":false})).collect();
        Self {
            model,
            instructions,
            input,
            tools,
            tool_choice: "auto",
            parallel_tool_calls: false,
            store: false,
            stream: true,
            include: Vec::new(),
            reasoning: reasoning_effort.map(|effort| json!({"effort": effort, "summary": "auto"})),
        }
    }
}

#[cfg(test)]
mod tests;
