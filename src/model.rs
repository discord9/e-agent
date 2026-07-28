use anyhow::Context;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::agent::{
    AssistantMessage, Message, Model, ModelDeltaKind, ToolCall, ToolSpec, Usage, preview,
};

#[derive(Clone)]
pub struct OpenAiModel {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
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
        Self::with_timeout(base_url, api_key, model, Duration::from_secs(600))
    }

    fn with_timeout(
        base_url: String,
        api_key: String,
        model: String,
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
        })
    }
}

#[async_trait::async_trait]
impl Model for OpenAiModel {
    async fn complete(
        &mut self,
        messages: &[Message],
        tools: &[ToolSpec],
        mut on_delta: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        let request = ChatRequest::from_internal(&self.model, messages, tools);
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
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("provider returned HTTP {status}: {}", preview(&body, 500));
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
                if let Some(delta) = choice.delta.content {
                    content.push_str(&delta);
                    if let Some(callback) = &mut on_delta {
                        callback(
                            ModelDeltaKind::Content,
                            &content[content.len() - delta.len()..],
                        );
                    }
                }
                if let Some(delta) = choice.delta.reasoning_content {
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
    messages: Vec<WireMessage>,
    tools: Vec<WireTool<'a>>,
    stream: bool,
}

impl<'a> ChatRequest<'a> {
    fn from_internal(model: &str, messages: &[Message], tools: &'a [ToolSpec]) -> Self {
        Self {
            model: model.into(),
            messages: messages.iter().map(WireMessage::from_internal).collect(),
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

#[derive(Debug, Serialize)]
struct WireMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<WireToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

impl WireMessage {
    fn from_internal(message: &Message) -> Self {
        match message {
            Message::System { content } => Self {
                role: "system",
                content: Some(content.clone()),
                tool_calls: None,
                tool_call_id: None,
            },
            Message::User { content } => Self {
                role: "user",
                content: Some(content.clone()),
                tool_calls: None,
                tool_call_id: None,
            },
            Message::Assistant(message) => Self {
                role: "assistant",
                content: message.content.clone(),
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
                content: Some(if *is_error {
                    format!("ERROR: {content}")
                } else {
                    content.clone()
                }),
                tool_calls: None,
                tool_call_id: Some(call_id.clone()),
            },
        }
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
                },
            ],
            &tools,
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
            &[Message::Assistant(AssistantMessage {
                content: Some("done".into()),
                tool_calls: vec![],
                reasoning: None,
            })],
            &[],
        );
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["messages"][0]["content"], "done");
        assert!(value["messages"][0].get("tool_calls").is_none());
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
            Duration::from_secs(1),
        )
        .unwrap();
        let mut deltas = Vec::new();
        let (message, usage) = model
            .complete(
                &[Message::User {
                    content: "hello".into(),
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
            &[Message::Assistant(AssistantMessage {
                content: Some("done".into()),
                tool_calls: vec![],
                reasoning: Some("secret thinking".into()),
            })],
            &[],
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
            Duration::from_millis(50),
        )
        .unwrap();
        let error = model
            .complete(
                &[Message::User {
                    content: "hello".into(),
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
            Duration::from_millis(100),
        )
        .unwrap();
        let (message, _) = model
            .complete(
                &[Message::User {
                    content: "hello".into(),
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
}
