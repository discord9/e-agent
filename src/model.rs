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
    messages: Vec<WireMessage>,
    tools: Vec<WireTool<'a>>,
    stream: bool,
}

impl<'a> ChatRequest<'a> {
    fn from_internal(
        model: &str,
        reasoning_effort: Option<&str>,
        messages: &[Message],
        tools: &'a [ToolSpec],
        image_store: Option<&Path>,
    ) -> Self {
        Self {
            model: model.into(),
            reasoning_effort: reasoning_effort.map(str::to_owned),
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
mod tests {
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
        let request = ChatRequest::from_internal("test-model", Some("max"), &[], &[], None);
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["reasoning_effort"], "max");
    }

    #[test]
    fn omits_reasoning_effort_when_unset() {
        let request = ChatRequest::from_internal("test-model", None, &[], &[], None);
        let value = serde_json::to_value(request).unwrap();
        assert!(value.get("reasoning_effort").is_none());
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
}
