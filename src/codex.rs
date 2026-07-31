use anyhow::{Context, anyhow, bail};
use base64::Engine;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

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
        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&access_token)
            .header("ChatGPT-Account-ID", account_id)
            .header("originator", "codex_cli_rs")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .header(reqwest::header::USER_AGENT, "codex_cli_rs/0.0.0 (e-agent)")
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
        crate::agent::ensure_vision_supported(&self.model, self.vision, messages)?;
        let request = ResponsesRequest::from_internal(
            &self.model,
            self.reasoning_effort.as_deref(),
            messages,
            tools,
            self.image_store.as_deref(),
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
                "response.reasoning_summary_part.added" if !reasoning.is_empty() => {
                    reasoning.push_str("\n\n");
                    if let Some(callback) = &mut on_delta {
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
