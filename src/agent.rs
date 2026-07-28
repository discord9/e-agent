use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use tokio::sync::mpsc;

pub const MAX_TOOL_ROUNDS: usize = 32;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    /// Model reasoning, persisted for display/audit only. Never sent back
    /// to the provider (see WireMessage).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, PartialEq)]
pub enum AgentEvent {
    AssistantText(String),
    AssistantDelta(String),
    ReasoningDelta(String),
    ToolCall { name: String, arguments: String },
    ToolResult { is_error: bool, content: String },
    BackgroundCompleted { id: u64, output: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelDeltaKind {
    Content,
    Reasoning,
}

#[async_trait]
pub trait Model: Send {
    async fn complete(
        &mut self,
        messages: &[Message],
        tools: &[ToolSpec],
        on_delta: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<AssistantMessage>;
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, arguments: Value) -> Result<String, String>;
    fn set_event_sender(&mut self, _sender: mpsc::UnboundedSender<AgentEvent>) {}
}

pub struct Agent {
    model: Box<dyn Model>,
    tools: Vec<Box<dyn Tool>>,
    transcript: Vec<Message>,
    event_handler: Option<Box<dyn FnMut(AgentEvent) + Send>>,
    max_tool_rounds: usize,
    background_receiver: mpsc::UnboundedReceiver<AgentEvent>,
    pending_background: VecDeque<(u64, String)>,
    subscriber: Option<mpsc::UnboundedSender<AgentEvent>>,
    running_background: Option<u64>,
}

impl Agent {
    pub fn new(model: Box<dyn Model>, mut tools: Vec<Box<dyn Tool>>) -> Self {
        let (background_sender, background_receiver) = mpsc::unbounded_channel();
        for tool in &mut tools {
            tool.set_event_sender(background_sender.clone());
        }
        Self {
            model,
            tools,
            transcript: Vec::new(),
            event_handler: None,
            max_tool_rounds: MAX_TOOL_ROUNDS,
            background_receiver,
            pending_background: VecDeque::new(),
            subscriber: None,
            running_background: None,
        }
    }

    pub fn max_tool_rounds(mut self, rounds: usize) -> Self {
        self.max_tool_rounds = rounds;
        self
    }

    pub fn set_event_handler(&mut self, handler: Box<dyn FnMut(AgentEvent) + Send>) {
        self.event_handler = Some(handler);
    }

    pub fn transcript(&self) -> &[Message] {
        &self.transcript
    }

    pub fn restore_transcript(&mut self, transcript: Vec<Message>) {
        self.transcript = transcript;
    }

    pub fn subscribe(&mut self, sender: mpsc::UnboundedSender<AgentEvent>) {
        self.subscriber = Some(sender);
    }

    pub fn background_task_id(&self) -> Option<u64> {
        self.running_background
    }

    pub async fn run(&mut self, prompt: String) -> anyhow::Result<String> {
        self.drain_background();
        self.inject_pending_background();
        self.transcript.push(Message::User { content: prompt });
        let specs: Vec<_> = self.tools.iter().map(|tool| tool.spec()).collect();

        let result = self.run_loop(&specs).await;
        self.drain_background();
        self.subscriber = None;
        result
    }

    /// Summarize everything before the current turn (the messages after the
    /// last user prompt) into a labelled user message. The current turn is
    /// kept verbatim, which also guarantees the kept tail never starts with
    /// an orphaned tool result or unmatched tool call.
    pub async fn compact(&mut self) -> anyhow::Result<String> {
        let Some(split) = self
            .transcript
            .iter()
            .rposition(|message| matches!(message, Message::User { .. }))
        else {
            anyhow::bail!("nothing to compact");
        };
        if split == 0 {
            anyhow::bail!("nothing to compact");
        }
        let mut request = self.transcript[..split].to_vec();
        request.push(Message::User {
            content: "Summarize the earlier conversation. Preserve the user's goals, decisions made, files changed, and unfinished work. Be concise and use Chinese or English to match the conversation language.".into(),
        });
        let response = {
            let model = &mut self.model;
            let event_handler = &mut self.event_handler;
            let mut on_delta = |kind: ModelDeltaKind, delta: &str| {
                if let Some(handler) = event_handler {
                    handler(match kind {
                        ModelDeltaKind::Content => AgentEvent::AssistantDelta(delta.into()),
                        ModelDeltaKind::Reasoning => AgentEvent::ReasoningDelta(delta.into()),
                    });
                }
            };
            model.complete(&request, &[], Some(&mut on_delta)).await?
        };
        let summary = response.content.unwrap_or_default();
        let mut compacted = vec![Message::User {
            content: format!("[compacted summary of earlier conversation]\n{summary}"),
        }];
        compacted.extend_from_slice(&self.transcript[split..]);
        self.transcript = compacted;
        Ok(summary)
    }

    async fn run_loop(&mut self, specs: &[ToolSpec]) -> anyhow::Result<String> {
        for _ in 0..self.max_tool_rounds {
            self.drain_background();
            self.inject_pending_background();
            let mut produced_delta = false;
            let assistant = {
                let model = &mut self.model;
                let event_handler = &mut self.event_handler;
                let mut on_delta = |kind: ModelDeltaKind, delta: &str| {
                    if kind == ModelDeltaKind::Content {
                        produced_delta = true;
                    }
                    if let Some(handler) = event_handler {
                        handler(match kind {
                            ModelDeltaKind::Content => AgentEvent::AssistantDelta(delta.into()),
                            ModelDeltaKind::Reasoning => AgentEvent::ReasoningDelta(delta.into()),
                        });
                    }
                };
                model
                    .complete(&self.transcript, specs, Some(&mut on_delta))
                    .await?
            };
            if assistant.tool_calls.is_empty() {
                let answer = assistant.content.clone().unwrap_or_default();
                self.transcript.push(Message::Assistant(assistant));
                return Ok(answer);
            }

            if !produced_delta
                && let Some(content) = assistant
                    .content
                    .as_deref()
                    .filter(|content| !content.is_empty())
            {
                self.emit(AgentEvent::AssistantText(content.into()));
            }
            self.transcript.push(Message::Assistant(assistant.clone()));
            for call in &assistant.tool_calls {
                self.emit(AgentEvent::ToolCall {
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                });
                let result = self.execute_call(call).await;
                if result.is_ok() && call.name == "bash" && is_background_call(call) {
                    self.running_background =
                        started_task_id(result.as_deref().unwrap_or_default());
                }
                self.emit(AgentEvent::ToolResult {
                    is_error: result.is_err(),
                    content: match &result {
                        Ok(content) | Err(content) => content.clone(),
                    },
                });
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

        anyhow::bail!("tool call limit ({}) reached", self.max_tool_rounds)
    }

    async fn execute_call(&self, call: &ToolCall) -> Result<String, String> {
        let arguments = serde_json::from_str(&call.arguments)
            .map_err(|error| format!("invalid JSON arguments: {error}"))?;
        let Some(tool) = self.tools.iter().find(|tool| tool.spec().name == call.name) else {
            return Err(format!("unknown tool: {}", call.name));
        };
        tool.execute(arguments).await
    }

    fn emit(&mut self, event: AgentEvent) {
        if let Some(handler) = &mut self.event_handler {
            handler(event);
        }
    }

    fn inject_pending_background(&mut self) {
        while let Some((id, output)) = self.pending_background.pop_front() {
            self.transcript.push(Message::User {
                content: format!("[background task {id} completed]\n{output}"),
            });
        }
    }

    fn drain_background(&mut self) {
        while let Ok(AgentEvent::BackgroundCompleted { id, output }) =
            self.background_receiver.try_recv()
        {
            if self.running_background == Some(id) {
                self.running_background = None;
            }
            let event = AgentEvent::BackgroundCompleted {
                id,
                output: output.clone(),
            };
            self.pending_background.push_back((id, output));
            if let Some(subscriber) = &self.subscriber {
                let _ = subscriber.send(event);
            }
        }
    }
}

fn is_background_call(call: &ToolCall) -> bool {
    serde_json::from_str::<Value>(&call.arguments)
        .ok()
        .and_then(|value| value.get("background").and_then(Value::as_bool))
        .unwrap_or(false)
}

fn started_task_id(output: &str) -> Option<u64> {
    output
        .strip_prefix("started background task ")?
        .split_once(':')?
        .0
        .parse()
        .ok()
}

pub fn preview(text: &str, max_chars: usize) -> String {
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
            _: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
        ) -> anyhow::Result<AssistantMessage> {
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

    struct ScriptedBackgroundTool {
        sender: Option<mpsc::UnboundedSender<AgentEvent>>,
    }

    #[async_trait]
    impl Tool for ScriptedBackgroundTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "bash".into(),
                description: "background test".into(),
                parameters: json!({"type": "object"}),
            }
        }

        async fn execute(&self, arguments: Value) -> Result<String, String> {
            assert_eq!(arguments["background"], true);
            let sender = self.sender.clone().unwrap();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                let _ = sender.send(AgentEvent::BackgroundCompleted {
                    id: 1,
                    output: "exit code: 0\nstdout:\ndone\nstderr:\n".into(),
                });
            });
            Ok("started background task 1: echo done".into())
        }

        fn set_event_sender(&mut self, sender: mpsc::UnboundedSender<AgentEvent>) {
            self.sender = Some(sender);
        }
    }

    struct SlowEchoTool;

    #[async_trait]
    impl Tool for SlowEchoTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "slow_echo".into(),
                description: "sleeps, then echoes".into(),
                parameters: json!({"type": "object"}),
            }
        }

        async fn execute(&self, arguments: Value) -> Result<String, String> {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok(arguments["value"].to_string())
        }
    }

    struct DeltaModel {
        calls: usize,
    }

    #[async_trait]
    impl Model for DeltaModel {
        async fn complete(
            &mut self,
            _: &[Message],
            _: &[ToolSpec],
            mut on_delta: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
        ) -> anyhow::Result<AssistantMessage> {
            self.calls += 1;
            if self.calls == 1 {
                if let Some(callback) = &mut on_delta {
                    callback(ModelDeltaKind::Reasoning, "thinking");
                    callback(ModelDeltaKind::Content, "streamed");
                }
                return Ok(AssistantMessage {
                    content: Some("streamed".into()),
                    tool_calls: vec![call("call-1", "echo", r#"{"value":"ok"}"#)],
                    reasoning: None,
                });
            }
            Ok(AssistantMessage {
                content: Some("final".into()),
                tool_calls: vec![],
                reasoning: None,
            })
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
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("final answer".into()),
                    tool_calls: vec![],
                    reasoning: None,
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
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("second".into()),
                    tool_calls: vec![],
                    reasoning: None,
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
                    reasoning: None,
                },
                AssistantMessage {
                    content: None,
                    tool_calls: vec![call("failed", "fail", "{}")],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("recovered".into()),
                    tool_calls: vec![],
                    reasoning: None,
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

    #[tokio::test]
    async fn emits_assistant_tool_and_result_events_in_order() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let model = ScriptedModel {
            replies: vec![
                AssistantMessage {
                    content: Some("working".into()),
                    tool_calls: vec![call("call-1", "echo", r#"{"value":"ok"}"#)],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("final".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
            ],
            requests,
        };
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut agent = Agent::new(Box::new(model), vec![Box::new(EchoTool)]);
        let captured = events.clone();
        agent.set_event_handler(Box::new(move |event| captured.lock().unwrap().push(event)));

        assert_eq!(agent.run("hello".into()).await.unwrap(), "final");
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                AgentEvent::AssistantText("working".into()),
                AgentEvent::ToolCall {
                    name: "echo".into(),
                    arguments: r#"{"value":"ok"}"#.into(),
                },
                AgentEvent::ToolResult {
                    is_error: false,
                    content: "\"ok\"".into(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn emits_deltas_without_duplicate_assistant_text() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut agent = Agent::new(Box::new(DeltaModel { calls: 0 }), vec![Box::new(EchoTool)]);
        let captured = events.clone();
        agent.set_event_handler(Box::new(move |event| captured.lock().unwrap().push(event)));
        assert_eq!(agent.run("hello".into()).await.unwrap(), "final");
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                AgentEvent::ReasoningDelta("thinking".into()),
                AgentEvent::AssistantDelta("streamed".into()),
                AgentEvent::ToolCall {
                    name: "echo".into(),
                    arguments: r#"{"value":"ok"}"#.into(),
                },
                AgentEvent::ToolResult {
                    is_error: false,
                    content: "\"ok\"".into(),
                },
            ]
        );
        assert!(agent.transcript().iter().all(|message| !matches!(
            message,
            Message::Assistant(AssistantMessage { content: Some(content), .. }) if content.contains("thinking")
        )));
    }

    #[tokio::test]
    async fn injects_background_completion_before_the_next_prompt_and_forwards_it() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let model = ScriptedModel {
            replies: vec![
                AssistantMessage {
                    content: None,
                    tool_calls: vec![call(
                        "background-1",
                        "bash",
                        r#"{"command":"echo done","background":true}"#,
                    )],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("started".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("next".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
            ],
            requests: requests.clone(),
        };
        let mut agent = Agent::new(
            Box::new(model),
            vec![Box::new(ScriptedBackgroundTool { sender: None })],
        );
        assert_eq!(agent.run("first".into()).await.unwrap(), "started");
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let (sender, mut receiver) = mpsc::unbounded_channel();
        agent.subscribe(sender);
        assert_eq!(agent.run("second".into()).await.unwrap(), "next");
        assert!(matches!(
            receiver.try_recv(),
            Ok(AgentEvent::BackgroundCompleted { id: 1, .. })
        ));
        let requests = requests.lock().unwrap();
        assert!(matches!(
            &requests[1][2],
            Message::Tool { content, is_error: false, .. } if content.starts_with("started background task 1:")
        ));
        assert!(matches!(
            &requests[2][4],
            Message::User { content } if content.starts_with("[background task 1 completed]\n")
        ));
        assert!(matches!(
            &requests[2][5],
            Message::User { content } if content == "second"
        ));
    }

    #[tokio::test]
    async fn injects_background_completion_mid_loop_before_the_next_model_call() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let model = ScriptedModel {
            replies: vec![
                AssistantMessage {
                    content: None,
                    tool_calls: vec![call(
                        "background-1",
                        "bash",
                        r#"{"command":"echo done","background":true}"#,
                    )],
                    reasoning: None,
                },
                AssistantMessage {
                    content: None,
                    tool_calls: vec![call("call-2", "slow_echo", r#"{"value":"ok"}"#)],
                    reasoning: None,
                },
                AssistantMessage {
                    content: Some("done".into()),
                    tool_calls: vec![],
                    reasoning: None,
                },
            ],
            requests: requests.clone(),
        };
        let mut agent = Agent::new(
            Box::new(model),
            vec![
                Box::new(ScriptedBackgroundTool { sender: None }),
                Box::new(SlowEchoTool),
            ],
        );

        assert_eq!(agent.run("go".into()).await.unwrap(), "done");
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(matches!(
            requests[2].last().unwrap(),
            Message::User { content } if content.starts_with("[background task 1 completed]\n")
        ));
    }

    #[tokio::test]
    async fn compacts_everything_before_the_current_turn() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let model = ScriptedModel {
            replies: vec![AssistantMessage {
                content: Some("summary text".into()),
                tool_calls: vec![],
                reasoning: None,
            }],
            requests: requests.clone(),
        };
        let tool_call = call("call-1", "echo", r#"{"value":"old"}"#);
        let current_turn = vec![
            Message::User {
                content: "recent request".into(),
            },
            Message::Assistant(AssistantMessage {
                content: None,
                tool_calls: vec![
                    call("call-2", "bash", r#"{"command":"make"}"#),
                    call("call-3", "bash", r#"{"command":"make test"}"#),
                ],
                reasoning: None,
            }),
            Message::Tool {
                call_id: "call-2".into(),
                name: "bash".into(),
                content: "building".into(),
                is_error: false,
            },
            Message::Tool {
                call_id: "call-3".into(),
                name: "bash".into(),
                content: "still building".into(),
                is_error: false,
            },
            Message::Assistant(AssistantMessage {
                content: Some("recent answer".into()),
                tool_calls: vec![],
                reasoning: None,
            }),
        ];
        let mut transcript = vec![
            Message::User {
                content: "original goal".into(),
            },
            Message::Assistant(AssistantMessage {
                content: None,
                tool_calls: vec![tool_call.clone()],
                reasoning: None,
            }),
            Message::Tool {
                call_id: tool_call.id,
                name: "echo".into(),
                content: "old result".into(),
                is_error: false,
            },
            Message::User {
                content: "follow up".into(),
            },
            Message::Assistant(AssistantMessage {
                content: Some("noted".into()),
                tool_calls: vec![],
                reasoning: None,
            }),
        ];
        transcript.extend(current_turn.clone());
        let mut agent = Agent::new(Box::new(model), vec![]);
        agent.restore_transcript(transcript);

        assert_eq!(agent.compact().await.unwrap(), "summary text");
        assert_eq!(agent.transcript().len(), current_turn.len() + 1);
        assert!(matches!(
            &agent.transcript()[0],
            Message::User { content } if content == "[compacted summary of earlier conversation]\nsummary text"
        ));
        assert_eq!(&agent.transcript()[1..], current_turn.as_slice());
        let requests = requests.lock().unwrap();
        assert_eq!(requests[0].len(), 6);
        assert!(matches!(
            requests[0].last().unwrap(),
            Message::User { content } if content.contains("Summarize the earlier conversation")
        ));
    }

    #[tokio::test]
    async fn refuses_to_compact_a_too_short_transcript() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let model = ScriptedModel {
            replies: vec![],
            requests,
        };
        let mut agent = Agent::new(Box::new(model), vec![]);
        agent.restore_transcript(vec![Message::User {
            content: "short".into(),
        }]);
        assert!(
            agent
                .compact()
                .await
                .unwrap_err()
                .to_string()
                .contains("nothing to compact")
        );
    }
}
