use anyhow::{Context, anyhow, bail};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};

use crate::agent::{
    AssistantMessage, Message, Model, ModelDeltaKind, ToolCall, ToolSpec, Usage, preview,
};
use crate::codex_auth::CodexAuth;

const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

#[derive(Clone)]
pub struct CodexModel {
    client: reqwest::Client,
    auth: CodexAuth,
    model: String,
    reasoning_effort: Option<String>,
    endpoint: String,
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
        })
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
        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&access_token)
            .header("ChatGPT-Account-ID", account_id)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .header(
                reqwest::header::USER_AGENT,
                format!("e-agent/{}", env!("CARGO_PKG_VERSION")),
            )
            .json(request)
            .send()
            .await
            .context("ChatGPT Responses request failed")?;
        Ok((response, access_token))
    }
}

#[async_trait::async_trait]
impl Model for CodexModel {
    fn name(&self) -> &str {
        &self.model
    }

    async fn complete(
        &mut self,
        messages: &[Message],
        tools: &[ToolSpec],
        mut on_delta: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        let request = ResponsesRequest::from_internal(
            &self.model,
            self.reasoning_effort.as_deref(),
            messages,
            tools,
        );
        let (mut response, rejected_access_token) = self.send(&request, true).await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            self.auth
                .refresh_after_unauthorized(&rejected_access_token)
                .await?;
            response = self.send(&request, false).await?.0;
        }
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!(
                "ChatGPT Responses returned HTTP {status}: {}",
                preview(&body, 500)
            );
        }
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
            let value: Value = serde_json::from_str(&event.data)
                .context("cannot decode ChatGPT Responses event")?;
            let kind = value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or(event.event.as_str());
            match kind {
                "response.output_text.delta" => {
                    if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                        content.push_str(delta);
                        if let Some(callback) = &mut on_delta {
                            callback(ModelDeltaKind::Content, delta);
                        }
                    }
                }
                "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                    if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                        reasoning.push_str(delta);
                        if let Some(callback) = &mut on_delta {
                            callback(ModelDeltaKind::Reasoning, delta);
                        }
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
            Message::User { content } => Some(json!({"type":"message","role":"user","content":[{"type":"input_text","text":content}]})),
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
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn responses_wire_flattens_system_tools_and_tool_results() {
        let request = ResponsesRequest::from_internal(
            "codex",
            Some("high"),
            &[
                Message::System {
                    content: "first".into(),
                },
                Message::System {
                    content: "second".into(),
                },
                Message::User {
                    content: "hello".into(),
                },
                Message::Assistant(AssistantMessage {
                    content: Some("working".into()),
                    tool_calls: vec![ToolCall {
                        id: "call-1".into(),
                        name: "bash".into(),
                        arguments: "{}".into(),
                    }],
                    reasoning: Some("never replayed".into()),
                }),
                Message::Tool {
                    call_id: "call-1".into(),
                    name: "bash".into(),
                    content: "failed".into(),
                    is_error: true,
                },
            ],
            &[ToolSpec {
                name: "bash".into(),
                description: "run".into(),
                parameters: json!({"type":"object"}),
            }],
        );
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["instructions"], "first\n\nsecond");
        assert_eq!(value["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(value["input"][2]["type"], "function_call");
        assert_eq!(value["input"][3]["output"], "ERROR: failed");
        assert_eq!(value["tools"][0]["name"], "bash");
        assert!(value["tools"][0].get("function").is_none());
        assert_eq!(value["reasoning"]["effort"], "high");
        assert_eq!(value["include"], json!([]));
    }

    #[test]
    fn usage_requires_completed_usage_fields() {
        assert_eq!(
            response_usage(&json!({"input_tokens": 3, "output_tokens": 4})).unwrap(),
            Usage {
                input_tokens: 3,
                output_tokens: 4
            }
        );
        assert!(response_usage(&json!({"input_tokens": 3})).is_err());
    }

    #[tokio::test]
    async fn local_sse_emits_text_reasoning_function_call_and_usage() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/responses", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let count = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..count]).to_ascii_lowercase();
            assert!(request.contains("chatgpt-account-id: account"));
            assert!(request.contains("user-agent: e-agent/"));
            let body = concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
                "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"why\"}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"bash\",\"arguments\":\"{}\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":4}}}\n\n"
            );
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
        });
        let temp = tempfile::tempdir().unwrap();
        let mut model = CodexModel::with_endpoint(
            crate::codex_auth::CodexAuth::test_auth(temp.path().join("auth.json")),
            endpoint,
        );
        let mut deltas = Vec::new();
        let (message, usage) = model
            .complete(
                &[Message::User {
                    content: "hello".into(),
                }],
                &[],
                Some(&mut |kind, text| deltas.push((kind, text.to_owned()))),
            )
            .await
            .unwrap();
        assert_eq!(message.content.as_deref(), Some("hi"));
        assert_eq!(message.reasoning.as_deref(), Some("why"));
        assert_eq!(message.tool_calls[0].id, "c1");
        assert_eq!(
            usage,
            Some(Usage {
                input_tokens: 3,
                output_tokens: 4
            })
        );
        assert_eq!(deltas.len(), 2);
    }

    async fn sse_failure(body: String) -> anyhow::Error {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/responses", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
        });
        let temp = tempfile::tempdir().unwrap();
        let mut model = CodexModel::with_endpoint(
            crate::codex_auth::CodexAuth::test_auth(temp.path().join("auth.json")),
            endpoint,
        );
        model
            .complete(
                &[Message::User {
                    content: "hello".into(),
                }],
                &[],
                None,
            )
            .await
            .unwrap_err()
    }

    #[tokio::test]
    async fn sse_reports_failed_incomplete_and_explicit_error_events() {
        let failed = sse_failure(
            "data: {\"type\":\"response.failed\",\"error\":{\"message\":\"failed reason\"}}\n\n"
                .into(),
        )
        .await;
        assert!(format!("{failed:#}").contains("failed reason"));
        let incomplete = sse_failure("data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n".into()).await;
        assert!(format!("{incomplete:#}").contains("max_output_tokens"));
        let explicit =
            sse_failure("data: {\"type\":\"error\",\"message\":\"explicit reason\"}\n\n".into())
                .await;
        assert!(format!("{explicit:#}").contains("explicit reason"));
        let response_error = sse_failure(
            "data: {\"type\":\"response.error\",\"error\":{\"message\":\"response reason\"}}\n\n"
                .into(),
        )
        .await;
        assert!(format!("{response_error:#}").contains("response reason"));
    }

    #[tokio::test]
    async fn sse_requires_completed_before_done_or_eof() {
        for body in [
            "data: [DONE]\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
        ] {
            let error = sse_failure(body.into()).await;
            assert!(format!("{error:#}").contains("before response.completed"));
        }
    }

    #[tokio::test]
    async fn backend_401_refreshes_once_persists_rotation_and_retries_once() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/responses", listener.local_addr().unwrap());
        let token_endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let count = first.read(&mut request).await.unwrap();
            assert!(
                String::from_utf8_lossy(&request[..count])
                    .to_ascii_lowercase()
                    .contains("authorization: bearer access")
            );
            first
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            let (mut token, _) = listener.accept().await.unwrap();
            let count = token.read(&mut request).await.unwrap();
            assert!(String::from_utf8_lossy(&request[..count]).contains("refresh"));
            let refresh = r#"{"access_token":"rotated-access","refresh_token":"rotated-refresh"}"#;
            token.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", refresh.len(), refresh).as_bytes()).await.unwrap();
            let (mut second, _) = listener.accept().await.unwrap();
            let count = second.read(&mut request).await.unwrap();
            assert!(
                String::from_utf8_lossy(&request[..count])
                    .to_ascii_lowercase()
                    .contains("authorization: bearer rotated-access")
            );
            let done = "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n\n";
            second.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", done.len(), done).as_bytes()).await.unwrap();
        });
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("auth.json");
        let auth = crate::codex_auth::CodexAuth::test_auth(path.clone())
            .with_token_endpoint(token_endpoint);
        let mut model = CodexModel::with_endpoint(auth.clone(), endpoint);
        let (_, usage) = model
            .complete(
                &[Message::User {
                    content: "hello".into(),
                }],
                &[],
                None,
            )
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(
            usage,
            Some(Usage {
                input_tokens: 1,
                output_tokens: 2
            })
        );
        assert_eq!(
            auth.current_access_token_and_account().await.unwrap().0,
            "rotated-access"
        );
        assert!(
            std::fs::read_to_string(path)
                .unwrap()
                .contains("rotated-refresh")
        );
    }
}
