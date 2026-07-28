use std::fmt;

use async_trait::async_trait;
use serde_json::Value;

pub const MAX_TOOL_ROUNDS: usize = 32;

#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    User {
        content: String,
    },
    Assistant(AssistantMessage),
    Tool {
        call_id: String,
        name: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct AssistantMessage {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug)]
pub struct ModelError(pub String);

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ModelError {}

#[async_trait]
pub trait Model: Send {
    async fn complete(
        &mut self,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<AssistantMessage, ModelError>;
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, arguments: Value) -> Result<String, String>;
}

pub struct Agent {
    model: Box<dyn Model>,
    tools: Vec<Box<dyn Tool>>,
    transcript: Vec<Message>,
    print_tool_calls: bool,
}

impl Agent {
    pub fn new(model: Box<dyn Model>, tools: Vec<Box<dyn Tool>>) -> Self {
        Self {
            model,
            tools,
            transcript: Vec::new(),
            print_tool_calls: false,
        }
    }

    pub fn print_tool_calls(mut self, enabled: bool) -> Self {
        self.print_tool_calls = enabled;
        self
    }

    pub async fn run(&mut self, prompt: String) -> Result<String, ModelError> {
        self.transcript.push(Message::User { content: prompt });
        let specs: Vec<_> = self.tools.iter().map(|tool| tool.spec()).collect();

        for _ in 0..MAX_TOOL_ROUNDS {
            let assistant = self.model.complete(&self.transcript, &specs).await?;
            if assistant.tool_calls.is_empty() {
                let answer = assistant.content.clone().unwrap_or_default();
                self.transcript.push(Message::Assistant(assistant));
                return Ok(answer);
            }

            self.transcript.push(Message::Assistant(assistant.clone()));
            for call in &assistant.tool_calls {
                if self.print_tool_calls {
                    eprintln!("tool: {} {}", call.name, preview(&call.arguments, 200));
                }
                let result = self.execute_call(call).await;
                if self.print_tool_calls {
                    match &result {
                        Ok(content) => eprintln!("  ok: {}", preview(content, 500)),
                        Err(error) => eprintln!("  error: {}", preview(error, 500)),
                    }
                }
                self.transcript.push(Message::Tool {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    content: match &result {
                        Ok(content) | Err(content) => content.clone(),
                    },
                    is_error: result.is_err(),
                });
            }
        }

        Err(ModelError(format!(
            "tool call limit ({MAX_TOOL_ROUNDS}) reached"
        )))
    }

    async fn execute_call(&self, call: &ToolCall) -> Result<String, String> {
        let arguments = serde_json::from_str(&call.arguments)
            .map_err(|error| format!("invalid JSON arguments: {error}"))?;
        let Some(tool) = self.tools.iter().find(|tool| tool.spec().name == call.name) else {
            return Err(format!("unknown tool: {}", call.name));
        };
        tool.execute(arguments).await
    }
}

fn preview(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;

    struct ScriptedModel {
        replies: Vec<AssistantMessage>,
        requests: Arc<Mutex<Vec<Vec<Message>>>>,
    }

    #[async_trait]
    impl Model for ScriptedModel {
        async fn complete(
            &mut self,
            messages: &[Message],
            _: &[ToolSpec],
        ) -> Result<AssistantMessage, ModelError> {
            self.requests.lock().unwrap().push(messages.to_vec());
            Ok(self.replies.remove(0))
        }
    }

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo".into(),
                description: "echoes input".into(),
                parameters: json!({"type": "object"}),
            }
        }

        async fn execute(&self, arguments: Value) -> Result<String, String> {
            Ok(arguments["value"].to_string())
        }
    }

    struct FailingTool;

    #[async_trait]
    impl Tool for FailingTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "fail".into(),
                description: "always fails".into(),
                parameters: json!({"type": "object"}),
            }
        }

        async fn execute(&self, _: Value) -> Result<String, String> {
            Err("execution failed".into())
        }
    }

    fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    #[tokio::test]
    async fn feeds_assistant_calls_and_results_back_to_the_model() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let model = ScriptedModel {
            replies: vec![
                AssistantMessage {
                    content: None,
                    tool_calls: vec![call("call-1", "echo", r#"{"value":"ok"}"#)],
                },
                AssistantMessage {
                    content: Some("final answer".into()),
                    tool_calls: vec![],
                },
            ],
            requests: requests.clone(),
        };
        let mut agent = Agent::new(Box::new(model), vec![Box::new(EchoTool)]);

        assert_eq!(agent.run("hello".into()).await.unwrap(), "final answer");
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(matches!(
            &requests[1][1],
            Message::Assistant(message) if message.tool_calls == vec![call("call-1", "echo", r#"{"value":"ok"}"#)]
        ));
        assert!(matches!(
            &requests[1][2],
            Message::Tool { call_id, name, content, is_error: false }
                if call_id == "call-1" && name == "echo" && content == "\"ok\""
        ));
    }

    #[tokio::test]
    async fn keeps_transcript_across_runs() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let model = ScriptedModel {
            replies: vec![
                AssistantMessage {
                    content: Some("first".into()),
                    tool_calls: vec![],
                },
                AssistantMessage {
                    content: Some("second".into()),
                    tool_calls: vec![],
                },
            ],
            requests: requests.clone(),
        };
        let mut agent = Agent::new(Box::new(model), vec![]);

        assert_eq!(agent.run("one".into()).await.unwrap(), "first");
        assert_eq!(agent.run("two".into()).await.unwrap(), "second");
        let requests = requests.lock().unwrap();
        assert_eq!(requests[0].len(), 1);
        assert_eq!(requests[1].len(), 3);
        assert!(matches!(
            &requests[1][0],
            Message::User { content } if content == "one"
        ));
        assert!(matches!(
            &requests[1][1],
            Message::Assistant(message) if message.content.as_deref() == Some("first")
        ));
        assert!(matches!(
            &requests[1][2],
            Message::User { content } if content == "two"
        ));
    }

    #[tokio::test]
    async fn returns_invalid_arguments_and_execution_failures_to_the_model() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let model = ScriptedModel {
            replies: vec![
                AssistantMessage {
                    content: None,
                    tool_calls: vec![call("bad-json", "fail", "not json")],
                },
                AssistantMessage {
                    content: None,
                    tool_calls: vec![call("failed", "fail", "{}")],
                },
                AssistantMessage {
                    content: Some("recovered".into()),
                    tool_calls: vec![],
                },
            ],
            requests: requests.clone(),
        };
        let mut agent = Agent::new(Box::new(model), vec![Box::new(FailingTool)]);

        assert_eq!(agent.run("hello".into()).await.unwrap(), "recovered");
        let requests = requests.lock().unwrap();
        assert!(matches!(
            &requests[1][2],
            Message::Tool { is_error: true, content, .. } if content.contains("invalid JSON")
        ));
        assert!(matches!(
            &requests[2][4],
            Message::Tool { is_error: true, content, .. } if content == "execution failed"
        ));
    }
}
