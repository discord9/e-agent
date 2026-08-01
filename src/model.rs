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
use crate::codex::CodexModel;

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
            messages,
            tools,
            self.image_store.as_deref(),
        );
        let mut retried = false;
        let response = loop {
            let result = self
                .client
                .post(format!("{}/chat/completions", self.base_url))
                .bearer_auth(&self.api_key)
                .json(&request)
                .send()
                .await;
            match result {
                // Transient gateway/connectivity hiccup before any bytes were
                // exchanged; safe to retry once.
                Err(error) if !retried && (error.is_timeout() || error.is_connect()) => {
                    retried = true;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                result => break result.map_err(request_error)?,
            }
        };
        let mut response = response;
        // Providers occasionally rate-limit a burst (main agent + a freshly
        // spawned subagent firing together) with a bare 403 and no body.
        // Back off and retry a couple of times before giving up.
        for attempt in 1..=3 {
            if response.status() != reqwest::StatusCode::FORBIDDEN {
                break;
            }
            tokio::time::sleep(Duration::from_millis(800 * attempt)).await;
            response = self
                .client
                .post(format!("{}/chat/completions", self.base_url))
                .bearer_auth(&self.api_key)
                .json(&request)
                .send()
                .await
                .map_err(request_error)?;
        }
        let status = response.status();
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
        let mut content = String::new();
        let mut reasoning = String::new();
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
                    if let Some(callback) = &mut on_delta {
                        callback(
                            ModelDeltaKind::Content,
                            &content[content.len() - delta.len()..],
                        );
                    }
                }
                if let Some(delta) = choice
                    .delta
                    .reasoning_content
                    .filter(|delta| !delta.is_empty())
                {
                    reasoning.push_str(&delta);
                    if let Some(callback) = &mut on_delta {
                        callback(ModelDeltaKind::Reasoning, &delta);
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
                    return build_assistant(content, reasoning, tool_calls, usage);
                }
            }
        }
        build_assistant(content, reasoning, tool_calls, usage)
    }
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

impl<'a> ChatRequest<'a> {
    fn from_internal(
        model: &str,
        reasoning_effort: Option<&str>,
        thinking: bool,
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
            messages: messages
                .iter()
                .map(|message| WireMessage::from_internal(message, image_store))
                .collect(),
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
}

impl WireMessage {
    fn from_internal(message: &Message, image_store: Option<&Path>) -> Self {
        match message {
            Message::System { content } => Self {
                role: "system",
                content: Some(WireContent::Text(content.clone())),
                tool_calls: None,
                tool_call_id: None,
            },
            Message::User { content, images } => Self {
                role: "user",
                content: Some(WireContent::from_user(content, images, image_store)),
                tool_calls: None,
                tool_call_id: None,
            },
            Message::Assistant(message) => Self {
                role: "assistant",
                content: message.content.clone().map(WireContent::Text),
                tool_calls: (!message.tool_calls.is_empty()).then(|| {
                    message
                        .tool_calls
                        .iter()
                        .map(WireToolCall::from_internal)
                        .collect()
                }),
                tool_call_id: None,
            },
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
    Ok((
        AssistantMessage {
            content: (!content.is_empty()).then_some(content),
            tool_calls,
            reasoning: (!reasoning.is_empty()).then_some(reasoning),
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
