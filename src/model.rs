use anyhow::Context;
use base64::Engine;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::agent::{
    AssistantMessage, ImagePart, Message, Model, ModelDeltaKind, ToolCall, ToolSpec, Usage, preview,
};
use crate::codex::{
    CONNECT_RETRY_ATTEMPTS, CONNECT_RETRY_BASE_BACKOFF_MS, CodexModel, DeltaSink,
    HTTP_RETRY_ATTEMPTS, HTTP_RETRY_BASE_BACKOFF_MS, is_retryable_http_status,
    is_stream_decode_error, retry_backoff_ms,
};

/// The two concrete model wires e-agent supports. Keeping this enum concrete
/// preserves the existing `Model` seam while making delegated clones exact.
#[derive(Clone)]
pub enum ConfiguredModelKind {
    Chat(OpenAiModel),
    Codex(CodexModel),
}

/// A configured model with an optional display name (profile key) for UI
/// rendering. `name()` always returns the wire model name; `display_name()`
/// returns the short part of the profile key (after the last '/') when set,
/// falling back to the wire name.
#[derive(Clone)]
pub struct ConfiguredModel {
    pub kind: ConfiguredModelKind,
    pub display: Option<String>,
}

impl ConfiguredModel {
    pub fn chat(model: OpenAiModel) -> Self {
        Self {
            kind: ConfiguredModelKind::Chat(model),
            display: None,
        }
    }

    pub fn codex(model: CodexModel) -> Self {
        Self {
            kind: ConfiguredModelKind::Codex(model),
            display: None,
        }
    }

    /// UI-friendly display name: the short part of the profile key (after the
    /// last '/') when configured, otherwise the wire model name.
    pub fn display_name(&self) -> &str {
        match self.display.as_deref() {
            Some(display) => display
                .rsplit_once('/')
                .map(|(_, short)| short)
                .filter(|short| !short.is_empty())
                .unwrap_or(display),
            None => self.name(),
        }
    }
}

#[async_trait::async_trait]
impl Model for ConfiguredModel {
    fn name(&self) -> &str {
        match &self.kind {
            ConfiguredModelKind::Chat(model) => model.name(),
            ConfiguredModelKind::Codex(model) => model.name(),
        }
    }

    fn supports_vision(&self) -> bool {
        match &self.kind {
            ConfiguredModelKind::Chat(model) => model.supports_vision(),
            ConfiguredModelKind::Codex(model) => model.supports_vision(),
        }
    }
    async fn complete(
        &mut self,
        messages: &[Message],
        tools: &[ToolSpec],
        on_delta: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        match &mut self.kind {
            ConfiguredModelKind::Chat(model) => model.complete(messages, tools, on_delta).await,
            ConfiguredModelKind::Codex(model) => model.complete(messages, tools, on_delta).await,
        }
    }
}

#[derive(Clone)]
pub struct OpenAiModel {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    reasoning_effort: Option<String>,
    /// Force the OpenAI-compatible `thinking` switch (`thinking:
    /// {"type": "enabled"}`) on chat requests. DeepSeek V4's `max`
    /// reasoning effort needs the explicit switch; `high` enables thinking
    /// by default. Other OpenAI-compatible providers ignore the field.
    thinking: bool,
    /// DeepSeek Chat wire compatibility (profile field `deepseek_compat =
    /// true`). Gates the two DeepSeek thinking-mode contract fixes, both
    /// verified against the official DeepSeek thinking-mode docs (and the
    /// DSH spec): (1) an assistant turn that carries `tool_calls` must echo
    /// its original `reasoning_content` back to the API on the next request
    /// or DeepSeek 400s; (2) such a content-less assistant turn must carry
    /// `content: ""` instead of an absent/null `content`. Defaults to false;
    /// other providers are byte-identical to the non-compat wire.
    deepseek_compat: bool,
    /// Whether the model accepts image input (chat wire builds image_url
    /// parts only for vision-capable models; see `ensure_vision_supported`).
    vision: bool,
    /// Global content-addressed image store (see `agent::image_store_dir`).
    /// None when no HOME/XDG_STATE_HOME: image refs then degrade to text
    /// placeholders on the wire.
    image_store: Option<PathBuf>,
}

impl OpenAiModel {
    pub fn from_env(base_url: Option<String>, model: Option<String>) -> anyhow::Result<Self> {
        let api_key = std::env::var("OPENAI_API_KEY").context("OPENAI_API_KEY is required")?;
        let base_url = base_url
            .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
            .unwrap_or_else(|| "https://api.openai.com/v1".into());
        let model = model
            .or_else(|| std::env::var("OPENAI_MODEL").ok())
            .unwrap_or_else(|| "gpt-4o-mini".into());
        Self::with_timeout(
            base_url,
            api_key,
            model,
            None,
            false,
            Duration::from_secs(600),
        )
    }

    pub fn new(
        base_url: String,
        api_key: String,
        model: String,
        reasoning_effort: Option<String>,
    ) -> anyhow::Result<Self> {
        Self::with_timeout(
            base_url,
            api_key,
            model,
            reasoning_effort,
            false,
            Duration::from_secs(600),
        )
    }

    /// Mark the model as vision-capable so user messages with attached
    /// images pass the vision gate.
    pub fn with_vision(mut self, vision: bool) -> Self {
        self.vision = vision;
        self
    }

    /// Force the OpenAI-compatible `thinking` switch on chat requests
    /// (`thinking: {"type": "enabled"}`), for providers whose reasoning
    /// mode is off by default (DeepSeek V4 `max`).
    pub fn with_thinking(mut self, thinking: bool) -> Self {
        self.thinking = thinking;
        self
    }

    /// Enable DeepSeek Chat wire compatibility (profile field
    /// `deepseek_compat = true`): replay `reasoning_content` on tool-call
    /// assistant turns when thinking is on, and wire content-less assistant
    /// turns as `content: ""` instead of absent/null. Solely for DeepSeek
    /// Chat thinking-mode; other providers must keep this false.
    pub fn with_deepseek_compat(mut self, deepseek_compat: bool) -> Self {
        self.deepseek_compat = deepseek_compat;
        self
    }

    #[cfg(test)]
    fn with_image_store(mut self, store: PathBuf) -> Self {
        self.image_store = Some(store);
        self
    }

    fn with_timeout(
        base_url: String,
        api_key: String,
        model: String,
        reasoning_effort: Option<String>,
        vision: bool,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .context("cannot create HTTP client")?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').into(),
            api_key,
            model,
            reasoning_effort,
            thinking: false,
            deepseek_compat: false,
            vision,
            image_store: crate::agent::image_store_dir(),
        })
    }

    /// Model identifier, e.g. for role routing hints in tool descriptions.
    pub fn name(&self) -> &str {
        &self.model
    }
}

#[async_trait::async_trait]
impl Model for OpenAiModel {
    fn name(&self) -> &str {
        self.name()
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
        let request = ChatRequest::from_internal(
            &self.model,
            self.reasoning_effort.as_deref(),
            self.thinking,
            self.deepseek_compat,
            messages,
            tools,
            self.image_store.as_deref(),
        );
        // Whole-request retry loop, mirroring the codex wire in src/codex.rs:
        // connect/timeout errors (no bytes exchanged) are retried inside
        // send_chat() — 8 attempts, 0.5s..32s exponential backoff (~63.5s
        // window) — while HTTP 403/5xx responses and SSE streams cut short
        // mid-flight (is_stream_decode_error) restart the whole idempotent
        // request here (HTTP_RETRY_ATTEMPTS, 0.8/1.6/3.2s backoff). Deltas
        // streamed to on_delta before a decode failure are discarded — only
        // the last successful attempt's accumulation is returned.
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let response = self.send_chat(&request).await?;
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
                anyhow::bail!(
                    "provider returned HTTP {status} (model={} effort={:?} msgs={} tools={}): {}",
                    self.model,
                    self.reasoning_effort,
                    messages.len(),
                    tools.len(),
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

/// Send a chat/completions request, retrying transient connect/timeout errors
/// (no bytes were exchanged: "tls handshake eof", ECONNREFUSED, TLS decrypt
/// send failures) with the shared exponential policy — 8 attempts,
/// 0.5s/1s/2s/4s/8s/16s/32s backoff in production, same as the codex wire.
/// HTTP status errors are returned to the caller, which owns 403/5xx retries.
impl OpenAiModel {
    async fn send_chat(&self, request: &ChatRequest<'_>) -> anyhow::Result<reqwest::Response> {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match self
                .client
                .post(format!("{}/chat/completions", self.base_url))
                .bearer_auth(&self.api_key)
                .json(request)
                .send()
                .await
            {
                Ok(response) => return Ok(response),
                Err(error)
                    if attempt < CONNECT_RETRY_ATTEMPTS
                        && (error.is_timeout() || error.is_connect() || error.is_request()) =>
                {
                    tokio::time::sleep(Duration::from_millis(retry_backoff_ms(
                        CONNECT_RETRY_BASE_BACKOFF_MS,
                        attempt,
                    )))
                    .await;
                }
                Err(error) => return Err(request_error(error)),
            }
        }
    }
}

/// Consume a chat/completions SSE stream, accumulating content / reasoning /
/// tool calls / usage into an assistant message. Returns Err for any stream
/// failure; the caller decides whether a decode failure is retryable
/// (`is_stream_decode_error`).
async fn consume_stream(
    response: reqwest::Response,
    on_delta: &mut DeltaSink<'_>,
) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
    let mut content = String::new();
    let mut reasoning = String::new();
    // Whether the stream ever carried a `reasoning_content` field (even an
    // empty one). DeepSeek emits `reasoning_content: ""` before the first
    // non-empty chunk; that presence must survive into the canonical
    // assistant (`Some("")`) so the compat wire can echo the field, while
    // the empty text itself must never surface as visible thinking.
    let mut saw_reasoning = false;
    let mut usage = None;
    let mut tool_calls: Vec<AccumulatedToolCall> = Vec::new();
    let mut events = response.bytes_stream().eventsource();
    while let Some(event) = events.next().await {
        let event = event.context("cannot parse provider event")?;
        if event.data == "[DONE]" {
            break;
        }
        let chunk: StreamChunk =
            serde_json::from_str(&event.data).context("cannot decode provider stream chunk")?;
        if let Some(error) = chunk.error {
            anyhow::bail!("provider stream error: {error}");
        }
        if let Some(reported) = chunk.usage {
            usage = Some(reported);
        }
        for choice in chunk.choices {
            if let Some(reported) = choice.usage {
                usage = Some(reported);
            }
            // Providers (kimi k3) interleave empty `content: ""` /
            // `reasoning_content: ""` chunks into the stream. Forwarding
            // them would flip the TUI's active stream lane and scatter a
            // single reasoning/content line into many fragments, so skip
            // empty deltas at the source.
            if let Some(delta) = choice.delta.content.filter(|delta| !delta.is_empty()) {
                content.push_str(&delta);
                if let Some(callback) = on_delta {
                    callback(
                        ModelDeltaKind::Content,
                        &content[content.len() - delta.len()..],
                    );
                }
            }
            // Reasoning deltas: an empty `reasoning_content: ""` chunk is a
            // real field presence (DeepSeek streams one before the first
            // non-empty chunk) but must not create visible thinking text —
            // no UI callback, no empty thinking block. Presence is tracked
            // separately so the canonical assistant can keep `Some("")`.
            if let Some(delta) = choice.delta.reasoning_content {
                saw_reasoning = true;
                if !delta.is_empty() {
                    reasoning.push_str(&delta);
                    if let Some(callback) = on_delta {
                        callback(ModelDeltaKind::Reasoning, &delta);
                    }
                }
            }
            for call in choice.delta.tool_calls.unwrap_or_default() {
                while tool_calls.len() <= call.index {
                    tool_calls.push(AccumulatedToolCall::default());
                }
                let target = &mut tool_calls[call.index];
                if let Some(id) = call.id {
                    target.id = id;
                }
                if let Some(name) = call
                    .function
                    .as_ref()
                    .and_then(|function| function.name.as_ref())
                {
                    target.name = name.clone();
                }
                if let Some(arguments) = call.function.and_then(|function| function.arguments) {
                    target.arguments.push_str(&arguments);
                }
            }
            if choice.finish_reason.is_some() {
                return build_assistant(content, reasoning, saw_reasoning, tool_calls, usage);
            }
        }
    }
    build_assistant(content, reasoning, saw_reasoning, tool_calls, usage)
}

fn request_error(error: reqwest::Error) -> anyhow::Error {
    if error.is_timeout() {
        anyhow::Error::new(error).context("provider request timed out")
    } else {
        anyhow::Error::new(error).context("provider request failed")
    }
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    /// DeepSeek V4 thinking switch (`thinking: {"type": "enabled"}`), sent
    /// as a top-level field when the profile sets `thinking = true` AND a
    /// `reasoning_effort` is present: DeepSeek's `max` mode requires the
    /// explicit switch (high enables thinking by default). Other
    /// OpenAI-compatible providers ignore unknown top-level fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Thinking>,
    messages: Vec<WireMessage>,
    tools: Vec<WireTool<'a>>,
    stream: bool,
}

/// The OpenAI-compatible thinking switch payload.
#[derive(Debug, Serialize)]
struct Thinking {
    r#type: &'static str,
}

/// A persisted assistant message with neither content nor tool calls is a
/// half-finished stream artifact (the provider silently dropped the
/// connection after reasoning-only deltas). Sending it makes the provider
/// reject the whole request with a permanent 400, so strip it on the wire to
/// rescue already-poisoned sessions. Safe: such a message necessarily has no
/// tool calls, so no later `tool` message depends on it (tool messages pair
/// by call_id), and reasoning is never replayed to the provider anyway
/// (AGENTS.md), so filtering loses no information.
fn is_poisoned_assistant(message: &Message) -> bool {
    matches!(
        message,
        Message::Assistant(assistant)
            if assistant.content.is_none() && assistant.tool_calls.is_empty()
    )
}

impl<'a> ChatRequest<'a> {
    fn from_internal(
        model: &str,
        reasoning_effort: Option<&str>,
        thinking: bool,
        deepseek_compat: bool,
        messages: &[Message],
        tools: &'a [ToolSpec],
        image_store: Option<&Path>,
    ) -> Self {
        Self {
            model: model.into(),
            reasoning_effort: reasoning_effort.map(str::to_owned),
            // Only force the switch when the profile opted in
            // (`thinking = true`) and a reasoning effort is set.
            thinking: reasoning_effort
                .filter(|_| thinking)
                .map(|_| Thinking { r#type: "enabled" }),
            messages: Self::wire_messages(messages, image_store, deepseek_compat, thinking),
            tools: tools
                .iter()
                .map(|tool| WireTool {
                    kind: "function",
                    function: WireFunction {
                        name: &tool.name,
                        description: &tool.description,
                        parameters: &tool.parameters,
                    },
                })
                .collect(),
            stream: true,
        }
    }
}

impl<'a> ChatRequest<'a> {
    /// Build the wire message list from the internal history. A run of
    /// consecutive `Message::Tool`s is serialized as valid `role:"tool"`
    /// text messages first (tool results must stay plain strings on the
    /// chat wire — images cannot ride on the tool role), then — when any
    /// tool in the run carried images — at most ONE aggregated temporary
    /// `role:"user"` wire message carries the whole run's image parts.
    /// That temporary message exists only on the wire: it never enters
    /// history, events, the store, or compaction.
    fn wire_messages(
        messages: &[Message],
        image_store: Option<&Path>,
        deepseek_compat: bool,
        thinking: bool,
    ) -> Vec<WireMessage> {
        let mut wire = Vec::new();
        let mut index = 0;
        while index < messages.len() {
            if matches!(&messages[index], Message::Tool { .. }) {
                let run_start = index;
                while index < messages.len() && matches!(&messages[index], Message::Tool { .. }) {
                    index += 1;
                }
                let run = &messages[run_start..index];
                // All valid role:tool text messages first.
                wire.extend(run.iter().map(|message| {
                    WireMessage::from_internal(message, image_store, deepseek_compat, thinking)
                }));
                // At most one aggregated temporary user image wire message
                // per batch.
                let images: Vec<ImagePart> = run
                    .iter()
                    .filter_map(|message| match message {
                        Message::Tool { images, .. } if !images.is_empty() => Some(images.clone()),
                        _ => None,
                    })
                    .flatten()
                    .collect();
                if !images.is_empty() {
                    wire.push(WireMessage {
                        role: "user",
                        content: Some(WireContent::from_user(
                            &format!("[image attached: {} image(s)]", images.len()),
                            &images,
                            image_store,
                        )),
                        tool_calls: None,
                        tool_call_id: None,
                        reasoning_content: None,
                    });
                }
            } else {
                let message = &messages[index];
                index += 1;
                if !is_poisoned_assistant(message) {
                    wire.push(WireMessage::from_internal(
                        message,
                        image_store,
                        deepseek_compat,
                        thinking,
                    ));
                }
            }
        }
        wire
    }
}

/// Chat-wire message content: a plain string for system/assistant/tool
/// messages, or a part array for user messages with attached images
/// (`text` + `image_url` parts; `image_url` is an OBJECT holding `url`).
/// `#[serde(untagged)]` keeps the wire output byte-identical to the old
/// string form whenever no images are attached.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum WireContent {
    Text(String),
    Parts(Vec<Value>),
}

#[derive(Debug, Serialize)]
struct WireMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<WireContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<WireToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    /// DeepSeek Chat thinking-mode only: the assistant's original
    /// `reasoning_content`, echoed back on a tool-call turn. Never set for
    /// any other provider or message role (matches the official DeepSeek
    /// thinking-mode docs: the field must participate in tool-call context
    /// concatenation; plain assistant reasoning stays out of the wire).
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
}

impl WireMessage {
    fn from_internal(
        message: &Message,
        image_store: Option<&Path>,
        deepseek_compat: bool,
        thinking: bool,
    ) -> Self {
        match message {
            Message::System { content } => Self {
                role: "system",
                content: Some(WireContent::Text(content.clone())),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            Message::User { content, images } => Self {
                role: "user",
                content: Some(WireContent::from_user(content, images, image_store)),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            Message::Assistant(message) => {
                let mut content = message.content.clone().map(WireContent::Text);
                let mut reasoning_content = None;
                if deepseek_compat {
                    // DeepSeek rejects assistant turns whose `content` is
                    // absent (HTTP 400 "missing field 'content'") when the
                    // message carries tool_calls; the official thinking-mode
                    // sample wires such turns as `content: ""`, not null.
                    if content.is_none() {
                        content = Some(WireContent::Text(String::new()));
                    }
                    // Thinking-mode tool-call rounds MUST echo the
                    // assistant's original `reasoning_content` back to the
                    // API on the next request, or DeepSeek returns the
                    // "reasoning_content must be passed back" 400. The
                    // field is echoed verbatim, including an empty string
                    // (`Some("")` is field presence, which DeepSeek also
                    // requires to round-trip). Plain assistant reasoning
                    // (no tool_calls) stays un-replayed.
                    if thinking
                        && !message.tool_calls.is_empty()
                        && let Some(reasoning) = message.reasoning.as_deref()
                    {
                        reasoning_content = Some(reasoning.to_owned());
                    }
                }
                Self {
                    role: "assistant",
                    content,
                    tool_calls: (!message.tool_calls.is_empty()).then(|| {
                        message
                            .tool_calls
                            .iter()
                            .map(WireToolCall::from_internal)
                            .collect()
                    }),
                    tool_call_id: None,
                    reasoning_content,
                }
            }
            Message::Tool {
                call_id,
                content,
                is_error,
                ..
            } => Self {
                role: "tool",
                content: Some(WireContent::Text(if *is_error {
                    format!("ERROR: {content}")
                } else {
                    content.clone()
                })),
                tool_calls: None,
                tool_call_id: Some(call_id.clone()),
                reasoning_content: None,
            },
        }
    }
}

impl WireContent {
    /// User content: plain text when no images are attached; otherwise an
    /// array of `text` + `image_url` parts. Image files are re-read from the
    /// global store and base64-encoded at send time; a missing file degrades
    /// to a `[image missing: <hash>]` text part instead of failing.
    fn from_user(content: &str, images: &[ImagePart], image_store: Option<&Path>) -> Self {
        if images.is_empty() {
            return Self::Text(content.to_owned());
        }
        let mut parts = vec![json!({"type": "text", "text": content})];
        for image in images {
            match crate::agent::load_image_bytes(image_store, &image.hash) {
                Some(bytes) => parts.push(json!({
                    "type": "image_url",
                    "image_url": {
                        "url": format!(
                            "data:{};base64,{}",
                            image.mime,
                            base64::engine::general_purpose::STANDARD.encode(bytes)
                        )
                    }
                })),
                None => parts.push(json!({
                    "type": "text",
                    "text": format!("[image missing: {}]", image.hash)
                })),
            }
        }
        Self::Parts(parts)
    }
}

#[derive(Debug, Serialize)]
struct WireTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunction<'a>,
}

#[derive(Debug, Serialize)]
struct WireFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WireToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: WireFunctionCall,
}

impl WireToolCall {
    fn from_internal(call: &ToolCall) -> Self {
        Self {
            id: call.id.clone(),
            kind: "function".into(),
            function: WireFunctionCall {
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WireFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Default)]
struct AccumulatedToolCall {
    id: String,
    name: String,
    arguments: String,
}

fn build_assistant(
    content: String,
    reasoning: String,
    saw_reasoning: bool,
    tool_calls: Vec<AccumulatedToolCall>,
    usage: Option<StreamUsage>,
) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
    let tool_calls = tool_calls
        .into_iter()
        .map(|call| {
            if call.id.is_empty() || call.name.is_empty() {
                anyhow::bail!("provider stream returned an incomplete tool call");
            }
            Ok(ToolCall {
                id: call.id,
                name: call.name,
                arguments: call.arguments,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    // A stream that ended with neither content nor tool calls is a
    // half-finished turn (e.g. the provider silently dropped the connection
    // after reasoning-only deltas). Building an assistant message anyway
    // would persist a message the provider later rejects with a permanent
    // 400, so bail and let the caller's retry/error path handle it instead.
    if content.is_empty() && tool_calls.is_empty() {
        anyhow::bail!("provider stream ended without content or tool calls (incomplete turn)");
    }
    Ok((
        AssistantMessage {
            content: (!content.is_empty()).then_some(content),
            tool_calls,
            // `Some("")` is meaningful field presence: DeepSeek streams
            // empty reasoning chunks, and the compat wire must echo the
            // field (even empty) on tool-call turns. Presence is tracked
            // separately from the accumulated text because empty deltas
            // never surface as visible thinking.
            reasoning: saw_reasoning.then_some(reasoning),
        },
        usage.map(|usage| Usage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
        }),
    ))
}

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    error: Option<serde_json::Value>,
    usage: Option<StreamUsage>,
}

#[derive(Clone, Copy, Deserialize)]
struct StreamUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
    finish_reason: Option<String>,
    usage: Option<StreamUsage>,
}

#[derive(Deserialize, Default)]
struct StreamDelta {
    content: Option<String>,
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<StreamToolCall>>,
}

#[derive(Deserialize)]
struct StreamToolCall {
    index: usize,
    id: Option<String>,
    function: Option<StreamFunction>,
}

#[derive(Deserialize)]
struct StreamFunction {
    name: Option<String>,
    arguments: Option<String>,
}

#[cfg(test)]
mod tests;
