use anyhow::{Context, anyhow};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::agent::{AssistantMessage, Message, Model, ToolCall, ToolSpec, preview};

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
        Self::with_timeout(base_url, api_key, model, Duration::from_secs(60))
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
    ) -> anyhow::Result<AssistantMessage> {
        let request = ChatRequest::from_internal(&self.model, messages, tools);
        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&request)
            .send()
            .await
            .map_err(request_error)?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("provider returned HTTP {status}: {}", preview(&body, 500));
        }
        response
            .json::<ChatResponse>()
            .await
            .context("cannot decode provider response")?
            .into_assistant()
    }
}

fn request_error(error: reqwest::Error) -> anyhow::Error {
    if error.is_timeout() {
        anyhow!("provider request timed out")
    } else {
        anyhow::Error::new(error).context("provider request failed")
    }
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: String,
    messages: Vec<WireMessage>,
    tools: Vec<WireTool<'a>>,
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

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
    tool_calls: Option<Vec<WireToolCall>>,
}

impl ChatResponse {
    fn into_assistant(self) -> anyhow::Result<AssistantMessage> {
        let message = self
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("provider response has no choices"))?
            .message;
        Ok(AssistantMessage {
            content: message.content,
            tool_calls: message
                .tool_calls
                .unwrap_or_default()
                .into_iter()
                .map(|call| ToolCall {
                    id: call.id,
                    name: call.function.name,
                    arguments: call.function.arguments,
                })
                .collect(),
        })
    }
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
            })],
            &[],
        );
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["messages"][0]["content"], "done");
        assert!(value["messages"][0].get("tool_calls").is_none());
    }

    #[test]
    fn parses_provider_tool_calls() {
        let response: ChatResponse = serde_json::from_value(json!({
            "choices": [{"message": {"content": null, "tool_calls": [{
                "id": "call-1", "type": "function",
                "function": {"name": "bash", "arguments": r#"{"command":"pwd"}"#}
            }]}}]
        }))
        .unwrap();
        let message = response.into_assistant().unwrap();
        assert_eq!(message.tool_calls[0].name, "bash");
        assert_eq!(message.tool_calls[0].arguments, r#"{"command":"pwd"}"#);
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

    async fn reply(stream: &mut TcpStream, body: serde_json::Value) {
        let body = serde_json::to_vec(&body).unwrap();
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(header.as_bytes()).await.unwrap();
        stream.write_all(&body).await.unwrap();
    }

    #[tokio::test]
    async fn request_times_out_when_server_stalls() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
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
            )
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("timed out"));
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
            reply(
                &mut first,
                json!({"choices": [{"message": {"content": null, "tool_calls": [{
                    "id": "call-fail", "type": "function",
                    "function": {"name": "fail", "arguments": "{}"}
                }]}}]}),
            )
            .await;
            let (mut second, _) = listener.accept().await.unwrap();
            let second_request = read_request(&mut second).await;
            reply(
                &mut second,
                json!({"choices": [{"message": {"content": "final answer"}}]}),
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
