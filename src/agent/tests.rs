use std::sync::{Arc, Mutex};

use serde_json::json;

use super::*;

struct ScriptedModel {
    replies: Vec<AssistantMessage>,
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
    /// Optional per-call latencies (popped in order; exhausted = no
    /// delay), to let a background completion land while a specific
    /// model call is in flight.
    delays: std::collections::VecDeque<Option<std::time::Duration>>,
}

#[async_trait]
impl Model for ScriptedModel {
    async fn complete(
        &mut self,
        messages: &[Message],
        _: &[ToolSpec],
        _: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        self.requests.lock().unwrap().push(messages.to_vec());
        if let Some(Some(delay)) = self.delays.pop_front() {
            tokio::time::sleep(delay).await;
        }
        Ok((self.replies.remove(0), None))
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
                label: None,
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
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        self.calls += 1;
        if self.calls == 1 {
            if let Some(callback) = &mut on_delta {
                callback(ModelDeltaKind::Reasoning, "thinking");
                callback(ModelDeltaKind::Content, "streamed");
            }
            return Ok((
                AssistantMessage {
                    content: Some("streamed".into()),
                    tool_calls: vec![call("call-1", "echo", r#"{"value":"ok"}"#)],
                    reasoning: None,
                },
                None,
            ));
        }
        Ok((
            AssistantMessage {
                content: Some("final".into()),
                tool_calls: vec![],
                reasoning: None,
            },
            None,
        ))
    }
}

fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.into(),
    }
}

#[test]
fn repair_tool_pairs_drops_orphan_results_after_a_synthetic_answer() {
    // Compaction captures a synthetic interrupted-result for call-1,
    // then the real result lands later (duplicate call_id). The second
    // Tool message has no pending tool_call and must be dropped.
    let messages = vec![
        Message::User {
            content: "u".into(),
            images: vec![],
        },
        Message::Assistant(AssistantMessage {
            content: None,
            tool_calls: vec![call("call-1", "bash", r#"{"cmd":"x"}"#)],
            reasoning: None,
        }),
        Message::Tool {
            call_id: "call-1".into(),
            name: "bash".into(),
            content: "[turn interrupted before a tool result was produced]".into(),
            is_error: true,
            synthetic: true,
        },
        Message::Tool {
            call_id: "call-1".into(),
            name: "bash".into(),
            content: "real result".into(),
            is_error: false,
            synthetic: false,
        },
    ];
    let repaired = repair_tool_pairs(messages);
    assert_eq!(repaired.len(), 3);
    // The synthetic placeholder is skipped so the real result can claim
    // the pending call: output = [User, Assistant(call-1), Tool(real)].
    assert!(matches!(
        &repaired[2],
        Message::Tool { call_id, content, is_error: false, synthetic: false, .. }
            if call_id == "call-1" && content == "real result"
    ));
    assert!(!repaired.iter().any(|m| matches!(
        m,
        Message::Tool {
            synthetic: true,
            ..
        }
    )));
}

#[test]
fn repair_tool_pairs_synthesizes_missing_results_in_order() {
    let messages = vec![
        Message::User {
            content: "u".into(),
            images: vec![],
        },
        Message::Assistant(AssistantMessage {
            content: None,
            tool_calls: vec![call("call-1", "bash", r#"{}"#)],
            reasoning: None,
        }),
        Message::User {
            content: "next".into(),
            images: vec![],
        },
    ];
    let repaired = repair_tool_pairs(messages);
    assert!(repaired.iter().any(|message| matches!(
        message,
        Message::Tool { call_id, is_error: true, synthetic: true, .. }
            if call_id == "call-1" && message_tool_content(message) == "[turn interrupted before a tool result was produced]"
    )));
    // The synthetic result must precede the following user message.
    let index = repaired
        .iter()
        .position(|m| matches!(m, Message::Tool { call_id, .. } if call_id == "call-1"))
        .unwrap();
    assert!(matches!(&repaired[index + 1], Message::User { .. }));
}

#[test]
fn repair_tool_pairs_keeps_real_result_matching_placeholder_text() {
    // A real tool result whose content happens to equal the interrupted
    // placeholder text must still pair normally: it is not skipped and
    // no placeholder is synthesized on top of it.
    let messages = vec![
        Message::User {
            content: "u".into(),
            images: vec![],
        },
        Message::Assistant(AssistantMessage {
            content: None,
            tool_calls: vec![call("call-1", "bash", r#"{}"#)],
            reasoning: None,
        }),
        Message::Tool {
            call_id: "call-1".into(),
            name: "bash".into(),
            content: "[turn interrupted before a tool result was produced]".into(),
            is_error: false,
            synthetic: false,
        },
    ];
    let repaired = repair_tool_pairs(messages);
    assert_eq!(repaired.len(), 3);
    assert!(matches!(
        &repaired[2],
        Message::Tool { call_id, content, is_error: false, synthetic: false, .. }
            if call_id == "call-1"
                && content == "[turn interrupted before a tool result was produced]"
    ));
    // Only that one real result exists: no placeholder was flushed.
    assert_eq!(
        repaired
            .iter()
            .filter(|m| matches!(
                m,
                Message::Tool { content, .. }
                    if *content == "[turn interrupted before a tool result was produced]"
            ))
            .count(),
        1
    );
    assert!(!repaired.iter().any(|m| matches!(
        m,
        Message::Tool {
            synthetic: true,
            ..
        }
    )));
}

#[test]
fn restore_history_migrates_legacy_interrupted_placeholders() {
    // A session file written before commit 92159c7: its interrupted-turn
    // placeholders — both a plain message entry and one inside a
    // compaction `retained` snapshot — carry no `synthetic` field and
    // deserialize as synthetic: false. restore_history must flag them so
    // repair_tool_pairs skips them instead of consuming them like real
    // results.
    let legacy_message = serde_json::json!({
        "type": "message",
        "message": {
            "Tool": {
                "call_id": "call-1",
                "name": "bash",
                "content": INTERRUPTED,
                "is_error": true,
            }
        }
    });
    let legacy_compaction = serde_json::json!({
        "type": "compaction",
        "summary": "old summary",
        "retained": [
            {
                "User": { "content": "current question" }
            },
            {
                "Assistant": {
                    "content": null,
                    "tool_calls": [
                        {
                            "id": "call-2",
                            "name": "bash",
                            "arguments": "{\"command\":\"make\"}"
                        }
                    ]
                }
            },
            {
                "Tool": {
                    "call_id": "call-2",
                    "name": "bash",
                    "content": INTERRUPTED,
                    "is_error": true,
                }
            }
        ]
    });
    let entries: Vec<SessionEntry> = vec![
        serde_json::from_value(legacy_message).unwrap(),
        serde_json::from_value(legacy_compaction).unwrap(),
    ];
    let mut agent = Agent::new(
        Box::new(ScriptedModel {
            replies: vec![],
            requests: Arc::new(Mutex::new(Vec::new())),
            delays: Default::default(),
        }),
        vec![],
    );
    agent.restore_history(entries);
    assert!(matches!(
        &agent.history()[0],
        SessionEntry::Message {
            message: Message::Tool {
                content,
                is_error: true,
                synthetic: true,
                ..
            }
        } if content == INTERRUPTED
    ));
    let SessionEntry::Compaction { retained, .. } = &agent.history()[1] else {
        panic!("expected compaction entry");
    };
    // The assistant turn is untouched; the legacy placeholder is flagged.
    assert!(matches!(&retained[1], Message::Assistant(_)));
    assert!(matches!(
        &retained[2],
        Message::Tool {
            content,
            is_error: true,
            synthetic: true,
            ..
        } if content == INTERRUPTED
    ));
}

#[test]
fn restore_history_migrates_legacy_placeholders_end_to_end() {
    // Full regression for the legacy gap: a pre-92159c7 session whose
    // compaction `retained` snapshot holds the assistant tool_call plus
    // the text-only interrupted placeholder, with the REAL result
    // persisted after the Compaction entry. After the load-time
    // migration, context() must skip the placeholder and pair the real
    // result — the model sees the real result, nothing is orphaned.
    let legacy_compaction = serde_json::json!({
        "type": "compaction",
        "summary": "old summary",
        "retained": [
            {
                "User": { "content": "current question" }
            },
            {
                "Assistant": {
                    "content": null,
                    "tool_calls": [
                        {
                            "id": "call-1",
                            "name": "bash",
                            "arguments": "{\"command\":\"make\"}"
                        }
                    ]
                }
            },
            {
                "Tool": {
                    "call_id": "call-1",
                    "name": "bash",
                    "content": INTERRUPTED,
                    "is_error": true,
                }
            }
        ]
    });
    let mut agent = Agent::new(
        Box::new(ScriptedModel {
            replies: vec![],
            requests: Arc::new(Mutex::new(Vec::new())),
            delays: Default::default(),
        }),
        vec![],
    );
    agent.restore_history(vec![
        serde_json::from_value(legacy_compaction).unwrap(),
        Message::Tool {
            call_id: "call-1".into(),
            name: "bash".into(),
            content: "real result".into(),
            is_error: false,
            synthetic: false,
        }
        .into(),
    ]);
    let context = agent.context();
    assert_eq!(context.len(), 4);
    assert!(matches!(
        &context[0],
        Message::User { content, .. }
            if content == "[compacted summary of earlier conversation]\nold summary"
    ));
    assert!(matches!(&context[1], Message::User { .. }));
    assert!(matches!(
        &context[2],
        Message::Assistant(message)
            if message.tool_calls == vec![call("call-1", "bash", r#"{"command":"make"}"#)]
    ));
    assert!(matches!(
        &context[3],
        Message::Tool { call_id, content, is_error: false, synthetic: false, .. }
            if call_id == "call-1" && content == "real result"
    ));
    // No placeholder reaches the provider, and nothing is left orphaned:
    // re-running the repair over the derived context is a fixed point.
    assert!(!context.iter().any(|m| matches!(
        m,
        Message::Tool {
            synthetic: true,
            ..
        }
    )));
    assert_eq!(repair_tool_pairs(context.clone()), context);
}

#[test]
fn context_pairs_real_result_across_a_compaction_snapshot() {
    // End-to-end (c): a compaction `retained` snapshot holds the
    // assistant tool_call plus its synthetic interrupted placeholder
    // (post-92159c7 shape); the real tool result landed in the history
    // AFTER the Compaction entry. context() must skip the placeholder
    // and pair the real result with its tool_call — no orphan, no
    // unpaired call (the 400-class malformation).
    let mut agent = Agent::new(
        Box::new(ScriptedModel {
            replies: vec![],
            requests: Arc::new(Mutex::new(Vec::new())),
            delays: Default::default(),
        }),
        vec![],
    );
    agent.restore_history(vec![
        SessionEntry::Compaction {
            summary: "summary text".into(),
            retained: vec![
                Message::User {
                    content: "current question".into(),
                    images: vec![],
                },
                Message::Assistant(AssistantMessage {
                    content: None,
                    tool_calls: vec![call("call-1", "bash", r#"{"command":"make"}"#)],
                    reasoning: None,
                }),
                Message::Tool {
                    call_id: "call-1".into(),
                    name: "bash".into(),
                    content: INTERRUPTED.into(),
                    is_error: true,
                    synthetic: true,
                },
            ],
        },
        Message::Tool {
            call_id: "call-1".into(),
            name: "bash".into(),
            content: "real result".into(),
            is_error: false,
            synthetic: false,
        }
        .into(),
    ]);
    let context = agent.context();
    assert_eq!(context.len(), 4);
    assert!(matches!(
        &context[0],
        Message::User { content, .. }
            if content == "[compacted summary of earlier conversation]\nsummary text"
    ));
    assert!(matches!(&context[1], Message::User { .. }));
    assert!(matches!(
        &context[2],
        Message::Assistant(message)
            if message.tool_calls == vec![call("call-1", "bash", r#"{"command":"make"}"#)]
    ));
    assert!(matches!(
        &context[3],
        Message::Tool { call_id, content, is_error: false, synthetic: false, .. }
            if call_id == "call-1" && content == "real result"
    ));
    assert!(!context.iter().any(|m| matches!(
        m,
        Message::Tool {
            synthetic: true,
            ..
        }
    )));
    assert_eq!(repair_tool_pairs(context.clone()), context);
}

fn message_tool_content(message: &Message) -> &str {
    match message {
        Message::Tool { content, .. } => content,
        _ => "",
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
        delays: Default::default(),
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
        Message::Tool { call_id, name, content, is_error: false, .. }
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
        delays: Default::default(),
    };
    let mut agent = Agent::new(Box::new(model), vec![]);

    assert_eq!(agent.run("one".into()).await.unwrap(), "first");
    assert_eq!(agent.run("two".into()).await.unwrap(), "second");
    let requests = requests.lock().unwrap();
    assert_eq!(requests[0].len(), 1);
    assert_eq!(requests[1].len(), 3);
    assert!(matches!(
        &requests[1][0],
        Message::User { content, .. } if content == "one"
    ));
    assert!(matches!(
        &requests[1][1],
        Message::Assistant(message) if message.content.as_deref() == Some("first")
    ));
    assert!(matches!(
        &requests[1][2],
        Message::User { content, .. } if content == "two"
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
        delays: Default::default(),
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
        delays: Default::default(),
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
    assert!(agent.context().iter().all(|message| !matches!(
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
        delays: Default::default(),
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
    // The completion is injected as a BackgroundCompletion, which
    // context() surfaces as a Message::User so the model sees it.
    assert!(matches!(
        &requests[2][4],
        Message::User { content, .. } if content.starts_with("[background task 1 completed]\n")
    ));
    assert!(matches!(
        &requests[2][5],
        Message::User { content, .. } if content == "second"
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
        delays: Default::default(),
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
        Message::User { content, .. } if content.starts_with("[background task 1 completed]\n")
    ));
}

#[tokio::test]
async fn compacts_everything_before_the_current_turn() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = ScriptedModel {
        replies: vec![AssistantMessage {
            content: Some("The user's original goal was to build the project; the assistant ran the make build and its tests through bash before moving on to the latest user turn.".into()),
            tool_calls: vec![],
            reasoning: None,
        }],
        requests: requests.clone(),
        delays: Default::default(),
    };
    let tool_call = call("call-1", "echo", r#"{"value":"old"}"#);
    let current_turn = vec![
        Message::User {
            content: "recent request".into(),
            images: vec![],
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
            synthetic: false,
        },
        Message::Tool {
            call_id: "call-3".into(),
            name: "bash".into(),
            content: "still building".into(),
            is_error: false,
            synthetic: false,
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
            images: vec![],
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
            synthetic: false,
        },
        Message::User {
            content: "follow up".into(),
            images: vec![],
        },
        Message::Assistant(AssistantMessage {
            content: Some("noted".into()),
            tool_calls: vec![],
            reasoning: None,
        }),
    ];
    transcript.extend(current_turn.clone());
    let mut agent = Agent::new(Box::new(model), vec![]);
    agent.restore_history(transcript.into_iter().map(Into::into).collect());

    assert_eq!(
        agent.compact().await.unwrap(),
        "The user's original goal was to build the project; the assistant ran the make build and its tests through bash before moving on to the latest user turn."
    );
    // Full history is append-only: 10 original entries + 1 compaction.
    assert_eq!(agent.history().len(), 11);
    assert!(matches!(
        agent.history().last().unwrap(),
        SessionEntry::Compaction { summary, retained }
            if summary == "The user's original goal was to build the project; the assistant ran the make build and its tests through bash before moving on to the latest user turn." && *retained == current_turn
    ));
    // The derived context is the summary plus the retained current turn.
    let context = agent.context();
    assert_eq!(context.len(), current_turn.len() + 1);
    assert!(matches!(
        &context[0],
        Message::User { content, .. } if content == "[compacted summary of earlier conversation]\nThe user's original goal was to build the project; the assistant ran the make build and its tests through bash before moving on to the latest user turn."
    ));
    assert_eq!(&context[1..], current_turn.as_slice());
    let requests = requests.lock().unwrap();
    assert_eq!(requests[0].len(), 6);
    assert!(matches!(
        requests[0].last().unwrap(),
        Message::User { content, .. } if content.contains("Summarize the earlier conversation")
    ));
}

#[tokio::test]
async fn context_repairs_unanswered_tool_calls_from_interrupted_turns() {
    let interrupted = vec![
        Message::User {
            content: "do things".into(),
            images: vec![],
        },
        Message::Assistant(AssistantMessage {
            content: None,
            tool_calls: vec![
                call("call-1", "bash", r#"{"command":"make"}"#),
                call("call-2", "bash", r#"{"command":"make test"}"#),
            ],
            reasoning: None,
        }),
        Message::Tool {
            call_id: "call-1".into(),
            name: "bash".into(),
            content: "built".into(),
            is_error: false,
            synthetic: false,
        },
    ];
    let mut agent = Agent::new(
        Box::new(ScriptedModel {
            replies: vec![],
            requests: Arc::new(Mutex::new(Vec::new())),
            delays: Default::default(),
        }),
        vec![],
    );
    agent.restore_history(interrupted.into_iter().map(Into::into).collect());
    let context = agent.context();
    assert_eq!(context.len(), 4);
    assert!(matches!(
        &context[3],
        Message::Tool { call_id, is_error: true, content, .. }
            if call_id == "call-2" && content.contains("interrupted")
    ));
}

#[tokio::test]
async fn refuses_to_compact_a_too_short_transcript() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = ScriptedModel {
        replies: vec![],
        requests,
        delays: Default::default(),
    };
    let mut agent = Agent::new(Box::new(model), vec![]);
    agent.restore_history(vec![
        Message::User {
            content: "short".into(),
            images: vec![],
        }
        .into(),
    ]);
    assert!(
        agent
            .compact()
            .await
            .unwrap_err()
            .to_string()
            .contains("nothing to compact")
    );
}

#[tokio::test]
async fn new_keeps_an_already_wired_event_sender() {
    // Tools with an explicit event sender retain it when Agent::new
    // attaches its default session sink.
    struct PreWired;
    #[async_trait]
    impl Tool for PreWired {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "prewired".into(),
                description: "already has a sender".into(),
                parameters: json!({"type": "object"}),
            }
        }
        async fn execute(&self, _: Value) -> Result<String, String> {
            Ok("ok".into())
        }
        fn set_event_sender(&mut self, _: mpsc::UnboundedSender<AgentEvent>) {
            panic!("Agent::new must not retarget a pre-wired tool");
        }
        fn has_event_sender(&self) -> bool {
            true
        }
    }
    let model = ScriptedModel {
        replies: vec![AssistantMessage {
            content: Some("done".into()),
            tool_calls: vec![],
            reasoning: None,
        }],
        requests: Arc::new(Mutex::new(Vec::new())),
        delays: Default::default(),
    };
    let mut agent = Agent::new(Box::new(model), vec![Box::new(PreWired)]);
    assert_eq!(agent.run("go".into()).await.unwrap(), "done");
}

#[tokio::test]
async fn records_and_clears_background_tasks_under_the_workspace() {
    // A background start is recorded on disk; its completion clears the
    // record, so only tasks that die WITH the process remain for the
    // next launch to report.
    let temp = tempfile::tempdir().unwrap();
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
                content: Some("reacted".into()),
                tool_calls: vec![],
                reasoning: None,
            },
        ],
        requests: Arc::new(Mutex::new(Vec::new())),
        delays: Default::default(),
    };
    let mut agent = Agent::new(
        Box::new(model),
        vec![Box::new(ScriptedBackgroundTool { sender: None })],
    );
    agent.record_background_tasks_in(
        temp.path().to_path_buf(),
        "test",
        crate::session_store::SessionStore::Jsonl,
    );
    assert_eq!(agent.run("go".into()).await.unwrap(), "started");
    // Task recorded while in flight (its completion arrives 10ms after
    // start; the first run finished before that).
    let record = temp.path().join(".e-agent/sessions/test.background.jsonl");
    assert!(record.exists());
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    // The follow-up run drains the completion and clears the record.
    assert_eq!(agent.run("next".into()).await.unwrap(), "reacted");
    assert!(!record.exists());
}

// ── Preview tests (middle-ellipsis) ──────────────────────────────────

#[test]
fn preview_short_text_unchanged() {
    assert_eq!(preview("hello", 10), "hello");
    assert_eq!(preview("hi", 2), "hi");
    assert_eq!(preview("", 5), "");
}

#[test]
fn preview_exact_fit() {
    assert_eq!(preview("abcde", 5), "abcde");
}

#[test]
fn preview_zero_or_one() {
    assert_eq!(preview("hello", 0), "");
    assert_eq!(preview("h", 1), "h");
    assert_eq!(preview("hello", 1), "\u{2026}");
}

#[test]
fn preview_max_2() {
    let r = preview("abcdef", 2);
    assert_eq!(r.chars().count(), 2);
    assert!(r.contains('\u{2026}'));
}

#[test]
fn preview_middle_ellipsis_ascii() {
    let r = preview("abcdefghijklmno", 10);
    assert_eq!(r.chars().count(), 10);
    assert!(r.contains('\u{2026}'));
    // 2:1 head:tail => head=6, tail=3 (available=9)
    assert!(r.starts_with("abcdef"), "head preserved, got {r:?}");
    assert!(r.ends_with("mno"), "tail preserved, got {r:?}");
}

#[test]
fn preview_middle_ellipsis_cjk() {
    let text = "你好世界数据驱动开发";
    let r = preview(text, 8);
    assert_eq!(r.chars().count(), 8);
    assert!(r.contains('\u{2026}'));
    // head=5 chars, tail=2 chars (available=7, 2:1 ratio)
    assert!(r.starts_with("你好世界"), "CJK head, got {r:?}");
    assert!(r.ends_with("开发"), "CJK tail, got {r:?}");
}

#[test]
fn preview_middle_ellipsis_emoji() {
    let text = "a😊b😊c😊d😊e😊f😊g";
    let r = preview(text, 8);
    assert_eq!(r.chars().count(), 8);
    assert!(
        r.contains('\u{2026}'),
        "emoji preview has ellipsis, got {r:?}"
    );
    // char-count respects Unicode, not bytes
}

#[test]
fn preview_char_count_never_exceeds_max() {
    for max in [3usize, 5, 10, 50, 100] {
        let text = "a".repeat(max * 2);
        let r = preview(&text, max);
        assert!(
            r.chars().count() <= max,
            "max={max}: actual {} > {max}, result: {r:?}",
            r.chars().count()
        );
    }
}

// ── BackgroundCompletion structured entry tests ──────────────────────

#[test]
fn background_completion_entry_serde_old_and_new() {
    // Old JSON without label → deserializes with label: None
    let old_json = r#"{"type":"background_completion","id":42,"output":"done"}"#;
    let deserialized: SessionEntry = serde_json::from_str(old_json).unwrap();
    assert!(
        matches!(
            deserialized,
            SessionEntry::BackgroundCompletion {
                id: 42,
                label: None,
                ..
            }
        ),
        "old payload must have label=None, got {deserialized:?}"
    );
    // Roundtrip with a label
    let entry = SessionEntry::BackgroundCompletion {
        id: 43,
        output: "done".into(),
        label: Some("build project".into()),
    };
    let json = serde_json::to_string(&entry).unwrap();
    assert!(
        json.contains(r#""label":"build project""#),
        "label must be present: {json}"
    );
    let deserialized: SessionEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized, entry);
    // Roundtrip with None label (serialized without label field)
    let entry_none = SessionEntry::BackgroundCompletion {
        id: 44,
        output: "done".into(),
        label: None,
    };
    let json_none = serde_json::to_string(&entry_none).unwrap();
    assert!(
        !json_none.contains("label"),
        "None label must be skipped: {json_none}"
    );
    let deserialized_none: SessionEntry = serde_json::from_str(&json_none).unwrap();
    assert_eq!(deserialized_none, entry_none);
}

#[test]
fn context_formats_background_completion_with_label_variants() {
    // Verify context() output for label=Some("build"), whitespace, and None.
    let cases: &[(&str, Option<&str>, &str)] = &[
        (
            "build",
            Some("build"),
            "[background task 7 completed: build]",
        ),
        ("", None, "[background task 7 completed]"),
        ("  ", None, "[background task 7 completed]"),
    ];
    for (_name, label_val, expected_header) in cases {
        let label = label_val.map(|s| s.to_string());
        let mut agent = Agent::new(
            Box::new(ScriptedModel {
                replies: vec![],
                requests: Arc::new(Mutex::new(Vec::new())),
                delays: Default::default(),
            }),
            vec![],
        );
        agent.restore_history(vec![SessionEntry::BackgroundCompletion {
            id: 7,
            output: "full output text\nwith multiple\nlines".into(),
            label,
        }]);
        let msgs = agent.context();
        assert_eq!(msgs.len(), 1);
        let expected = format!("{expected_header}\nfull output text\nwith multiple\nlines");
        assert!(
            matches!(&msgs[0], Message::User { content, .. } if content == &expected),
            "label={label_val:?}: expected {expected:?}, got {:?}",
            match &msgs[0] {
                Message::User { content, .. } => content,
                _ => "(wrong variant)",
            }
        );
    }
}

#[test]
fn background_completion_and_notice_coexist_in_context() {
    // Old Notice entries must still work alongside new BackgroundCompletion.
    let mut agent = Agent::new(
        Box::new(ScriptedModel {
            replies: vec![],
            requests: Arc::new(Mutex::new(Vec::new())),
            delays: Default::default(),
        }),
        vec![],
    );
    agent.restore_history(vec![
        SessionEntry::Notice {
            text: "[background task 1 completed]\nold style".into(),
        },
        SessionEntry::BackgroundCompletion {
            id: 2,
            output: "new style".into(),
            label: None,
        },
    ]);
    let msgs = agent.context();
    assert_eq!(msgs.len(), 2);
    assert!(matches!(
        &msgs[0],
        Message::User { content, .. } if content.contains("old style")
    ));
    assert!(matches!(
        &msgs[1],
        Message::User { content, .. } if content == "[background task 2 completed]\nnew style"
    ));
}

// ── fork_prefix ──────────────────────────────────────────────────────

fn completed_turn(question: &str, answer: &str) -> Vec<SessionEntry> {
    vec![
        Message::User {
            content: question.into(),
            images: vec![],
        }
        .into(),
        Message::Assistant(AssistantMessage {
            content: Some(answer.into()),
            tool_calls: vec![],
            reasoning: None,
        })
        .into(),
    ]
}

fn forked_history() -> Vec<SessionEntry> {
    let mut entries = completed_turn("q1", "a1");
    entries.extend(completed_turn("q2", "a2"));
    entries.extend(completed_turn("q3", "a3"));
    entries
}

#[test]
fn fork_prefix_default_cuts_at_last_completed_turn_and_drops_tail() {
    let mut entries = forked_history();
    // Trailing non-turn entries (Notice, BackgroundCompletion, another
    // ForkedFrom) must be dropped by the default fork point.
    entries.push(SessionEntry::Notice {
        text: "[background task 1 completed]\nzzz".into(),
    });
    entries.push(SessionEntry::BackgroundCompletion {
        id: 9,
        output: "output".into(),
        label: None,
    });
    entries.push(SessionEntry::ForkedFrom {
        source: "other".into(),
        at: 1,
        event_time: None,
        seq: None,
    });

    let prefix = fork_prefix(&entries, None).unwrap();
    assert_eq!(prefix, forked_history());
    // The boundary entry is the last assistant answer with no tool calls.
    assert!(is_turn_boundary(prefix.last().unwrap()));
}

#[test]
fn fork_prefix_at_is_1_based_inclusive() {
    let entries = forked_history();
    let prefix = fork_prefix(&entries, Some(4)).unwrap();
    assert_eq!(prefix.len(), 4);
    assert_eq!(
        prefix,
        completed_turn("q1", "a1")
            .into_iter()
            .chain(completed_turn("q2", "a2"))
            .collect::<Vec<_>>()
    );
    // Forking at the very last entry keeps everything.
    assert_eq!(fork_prefix(&entries, Some(6)).unwrap(), entries);
}

#[test]
fn fork_prefix_rejects_mid_turn_at() {
    let mut entries = forked_history();
    // Insert an assistant message that still has a pending tool call
    // (the turn is not complete at this entry).
    entries.push(
        Message::Assistant(AssistantMessage {
            content: None,
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            }],
            reasoning: None,
        })
        .into(),
    );
    let error = fork_prefix(&entries, Some(7)).unwrap_err();
    assert!(
        error.contains("not a turn boundary"),
        "mid-turn fork must be rejected, got {error:?}"
    );
    // Same entry is fine without an explicit at: the boundary search
    // stops at the previous completed turn.
    let prefix = fork_prefix(&entries, None).unwrap();
    assert_eq!(prefix.len(), 6);
}

#[test]
fn fork_prefix_rejects_out_of_range_at() {
    let entries = forked_history();
    let error = fork_prefix(&entries, Some(7)).unwrap_err();
    assert!(error.contains("out of range"), "{error}");
    let error = fork_prefix(&entries, Some(0)).unwrap_err();
    assert!(error.contains("out of range"), "0 is not 1-based: {error}");
}

#[test]
fn fork_prefix_rejects_empty_and_no_completed_turn() {
    assert_eq!(
        fork_prefix(&[], None).unwrap_err(),
        "no completed turn in session"
    );
    assert_eq!(
        fork_prefix(&[], Some(1)).unwrap_err(),
        "no completed turn in session"
    );
    // Only user messages: no assistant boundary anywhere.
    let no_turn = vec![
        Message::User {
            content: "q1".into(),
            images: vec![],
        }
        .into(),
        Message::User {
            content: "q2".into(),
            images: vec![],
        }
        .into(),
    ];
    assert_eq!(
        fork_prefix(&no_turn, None).unwrap_err(),
        "no completed turn in session"
    );
}

#[test]
fn fork_prefix_accepts_compaction_as_boundary() {
    let mut entries = forked_history();
    entries.push(SessionEntry::Compaction {
        summary: "summary".into(),
        retained: vec![],
    });
    let prefix = fork_prefix(&entries, None).unwrap();
    assert_eq!(prefix.len(), 7);
    assert!(matches!(
        prefix.last(),
        Some(SessionEntry::Compaction { .. })
    ));
    // Explicit at on the compaction works too.
    assert_eq!(fork_prefix(&entries, Some(7)).unwrap(), entries);
}

#[test]
fn forked_from_marker_serde_roundtrip_and_context_skip() {
    // Serialization: provenance None fields are skipped, at/source kept.
    let marker = SessionEntry::ForkedFrom {
        source: "src-123".into(),
        at: 4,
        event_time: Some(1_700_000_000_000_000),
        seq: Some(3),
    };
    let json = serde_json::to_string(&marker).unwrap();
    assert!(json.contains(r#""type":"forked_from""#), "{json}");
    assert!(json.contains(r#""source":"src-123""#), "{json}");
    assert!(json.contains(r#""at":4"#), "{json}");
    assert!(json.contains(r#""event_time":1700000000000000"#), "{json}");
    assert!(json.contains(r#""seq":3"#), "{json}");
    assert_eq!(serde_json::from_str::<SessionEntry>(&json).unwrap(), marker);
    let marker_none = SessionEntry::ForkedFrom {
        source: "src-123".into(),
        at: 4,
        event_time: None,
        seq: None,
    };
    let json_none = serde_json::to_string(&marker_none).unwrap();
    assert!(!json_none.contains("event_time"), "{json_none}");
    assert!(!json_none.contains("seq"), "{json_none}");
    assert_eq!(
        serde_json::from_str::<SessionEntry>(&json_none).unwrap(),
        marker_none
    );

    // context(): the marker must never reach the model wire.
    let mut agent = Agent::new(
        Box::new(ScriptedModel {
            replies: vec![],
            requests: Arc::new(Mutex::new(Vec::new())),
            delays: Default::default(),
        }),
        vec![],
    );
    agent.restore_history(vec![
        marker_none.clone(),
        Message::User {
            content: "q1".into(),
            images: vec![],
        }
        .into(),
        Message::Assistant(AssistantMessage {
            content: Some("a1".into()),
            tool_calls: vec![],
            reasoning: None,
        })
        .into(),
    ]);
    let msgs = agent.context();
    assert_eq!(msgs.len(), 2, "forked_from must not appear in context");
    assert!(matches!(
        &msgs[0],
        Message::User { content, .. } if content == "q1"
    ));
    assert!(!format!("{msgs:?}").contains("src-123"));
}

#[test]
fn user_images_serde_round_trips_and_old_sessions_load_without_images() {
    // New shape: images field round-trips.
    let message = Message::User {
        content: "look".into(),
        images: vec![ImagePart {
            hash: "abc123".into(),
            mime: "image/png".into(),
        }],
    };
    let json = serde_json::to_string(&message).unwrap();
    let back: Message = serde_json::from_str(&json).unwrap();
    assert_eq!(back, message);

    // Old sessions (no `images` key) deserialize with an empty list.
    let legacy: Message = serde_json::from_str(r#"{"User":{"content":"old"}}"#).unwrap();
    assert_eq!(
        legacy,
        Message::User {
            content: "old".into(),
            images: vec![],
        }
    );
    // And serialize back without the images key when empty.
    let legacy_json = serde_json::to_string(&legacy).unwrap();
    assert!(!legacy_json.contains("images"));
}

#[test]
fn split_image_marker_parses_hash_and_mime_and_strips_summary() {
    let (summary, image) = split_image_marker(
        "__EA_IMAGE__deadbeef,image/png__EA_IMAGE_END__[image read: a.png] (hash deadbeef, image/png, 4 bytes)",
    );
    assert_eq!(
        summary,
        "[image read: a.png] (hash deadbeef, image/png, 4 bytes)"
    );
    assert_eq!(
        image,
        Some(ImagePart {
            hash: "deadbeef".into(),
            mime: "image/png".into(),
        })
    );
    // Non-marker results pass through untouched.
    let (summary, image) = split_image_marker("plain result");
    assert_eq!(summary, "plain result");
    assert!(image.is_none());
    // Malformed markers are not mistaken for attachments.
    let (summary, image) = split_image_marker("__EA_IMAGE__nocomma__EA_IMAGE_END__rest");
    assert_eq!(summary, "__EA_IMAGE__nocomma__EA_IMAGE_END__rest");
    assert!(image.is_none());
}

struct ImageTool {
    workspace: tempfile::TempDir,
}

#[async_trait]
impl Tool for ImageTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_image".into(),
            description: "read an image".into(),
            parameters: json!({"type": "object"}),
        }
    }

    async fn execute(&self, _: Value) -> Result<String, String> {
        let store = self.workspace.path().join("store");
        let bytes = b"fake-png-bytes";
        let hash = store_image_bytes(&store, bytes).unwrap();
        Ok(format!(
            "{IMAGE_MARKER_START}{hash},image/png{IMAGE_MARKER_END}\
                 [image read: pics/cat.png] (hash {hash}, image/png, {} bytes)",
            bytes.len()
        ))
    }
}

struct ImageRoundModel {
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
    calls: usize,
    vision: bool,
}

#[async_trait]
impl Model for ImageRoundModel {
    fn supports_vision(&self) -> bool {
        self.vision
    }

    async fn complete(
        &mut self,
        messages: &[Message],
        _: &[ToolSpec],
        _: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        self.requests.lock().unwrap().push(messages.to_vec());
        self.calls += 1;
        if self.calls == 1 {
            return Ok((
                AssistantMessage {
                    content: None,
                    tool_calls: vec![call("call-img", "read_image", r#"{"path":"pics/cat.png"}"#)],
                    reasoning: None,
                },
                None,
            ));
        }
        Ok((
            AssistantMessage {
                content: Some("final".into()),
                tool_calls: vec![],
                reasoning: None,
            },
            None,
        ))
    }
}

#[tokio::test]
async fn run_loop_strips_marker_and_attaches_synthetic_user_with_image() {
    let temp = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::new(
        Box::new(ImageRoundModel {
            requests: requests.clone(),
            calls: 0,
            vision: true,
        }),
        vec![Box::new(ImageTool { workspace: temp })],
    );
    let answer = agent.run("describe".into()).await.unwrap();
    assert_eq!(answer, "final");
    let history = agent.history();
    // Tool result keeps only the text summary (no marker, no base64).
    let tool = history
        .iter()
        .find_map(|entry| match entry {
            SessionEntry::Message {
                message: Message::Tool { content, .. },
            } => Some(content.clone()),
            _ => None,
        })
        .unwrap();
    assert!(tool.starts_with("[image read: pics/cat.png]"));
    assert!(!tool.contains("__EA_IMAGE__"));
    assert!(!tool.contains("fake-png"));
    // The synthetic user message follows the tool result and carries the
    // image reference.
    let synthetic = history
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::Message {
                message: Message::User { content, images },
            } => Some((content.clone(), images.clone())),
            _ => None,
        })
        .find(|(content, _)| content.starts_with("[image attached:"));
    let (content, images) = synthetic.unwrap();
    assert_eq!(content, "[image attached: pics/cat.png]");
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].mime, "image/png");
    // And the model saw the image in the second round's context.
    let calls = requests.lock().unwrap();
    assert_eq!(calls.len(), 2);
    let second = &calls[1];
    assert!(second.iter().any(|message| matches!(
        message,
        Message::User { content, images } if content.starts_with("[image attached:")
            && !images.is_empty()
    )));
}

#[tokio::test]
async fn run_loop_skips_image_attachment_on_non_vision_models() {
    let temp = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::new(
        Box::new(ImageRoundModel {
            requests: requests.clone(),
            calls: 0,
            vision: false,
        }),
        vec![Box::new(ImageTool { workspace: temp })],
    );
    let answer = agent.run("describe".into()).await.unwrap();
    assert_eq!(answer, "final");
    let history = agent.history();
    // Tool result keeps the text summary, annotated that the attachment
    // was skipped (the vision gate would otherwise lock the session).
    let tool = history
        .iter()
        .find_map(|entry| match entry {
            SessionEntry::Message {
                message: Message::Tool { content, .. },
            } => Some(content.clone()),
            _ => None,
        })
        .unwrap();
    assert!(tool.starts_with("[image read: pics/cat.png]"));
    assert!(tool.contains("已跳过附加"));
    assert!(!tool.contains("__EA_IMAGE__"));
    // No synthetic user message with an image reference.
    let synthetic = history.iter().any(|entry| match entry {
        SessionEntry::Message {
            message: Message::User { content, images },
        } => content.starts_with("[image attached:") && !images.is_empty(),
        _ => false,
    });
    assert!(!synthetic);
    // The second round's context carries no images.
    let calls = requests.lock().unwrap();
    assert_eq!(calls.len(), 2);
    let second = &calls[1];
    assert!(
        second
            .iter()
            .all(|message| !matches!(message, Message::User { images, .. } if !images.is_empty()))
    );
}

/// `ScriptedModel` that reports vision support (compact/round requests keep
/// image parts — no stripping).
struct VisionScriptedModel(ScriptedModel);

#[async_trait]
impl Model for VisionScriptedModel {
    async fn complete(
        &mut self,
        messages: &[Message],
        tools: &[ToolSpec],
        on_delta: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        self.0.complete(messages, tools, on_delta).await
    }

    fn supports_vision(&self) -> bool {
        true
    }
}

/// Scripted model that simulates the wire vision gate: when `vision` is
/// false, any request carrying a `Message::User` image fails — exactly like
/// `ensure_vision_supported`. Proves a poisoned history would surface as a
/// failed call, so tests can assert recovery.
struct GateSimModel {
    replies: Vec<AssistantMessage>,
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
    vision: bool,
}

#[async_trait]
impl Model for GateSimModel {
    fn supports_vision(&self) -> bool {
        self.vision
    }

    async fn complete(
        &mut self,
        messages: &[Message],
        _: &[ToolSpec],
        _: Option<&mut (dyn for<'a> FnMut(ModelDeltaKind, &'a str) + Send)>,
    ) -> anyhow::Result<(AssistantMessage, Option<Usage>)> {
        self.requests.lock().unwrap().push(messages.to_vec());
        if !self.vision
            && messages
                .iter()
                .any(|m| matches!(m, Message::User { images, .. } if !images.is_empty()))
        {
            anyhow::bail!("model does not support image input");
        }
        Ok((self.replies.remove(0), None))
    }
}

#[tokio::test]
async fn non_vision_round_recovers_legacy_image_history() {
    // Legacy poisoned session: history carries an old image-bearing User
    // message and nothing else — split==0, so compaction cannot run. The
    // send-time cleanup in complete_round must still let the next round
    // succeed on a non-vision model (which simulates the wire gate by
    // erroring on image-bearing requests), while history keeps the image.
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::new(
        Box::new(GateSimModel {
            replies: vec![AssistantMessage {
                content: Some("ok".into()),
                tool_calls: vec![],
                reasoning: None,
            }],
            requests: requests.clone(),
            vision: false,
        }),
        vec![],
    );
    agent.restore_history(vec![
        Message::User {
            content: "old image".into(),
            images: vec![ImagePart {
                hash: "deadbeef".into(),
                mime: "image/png".into(),
            }],
        }
        .into(),
    ]);
    // Compaction refuses (nothing to compact)…
    assert!(
        agent
            .compact()
            .await
            .unwrap_err()
            .to_string()
            .contains("nothing to compact")
    );
    // …but a normal round succeeds: the request is stripped at send time.
    let answer = agent.run("look".into()).await.unwrap();
    assert_eq!(answer, "ok");
    let calls = requests.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].iter().all(|message| !matches!(
        message,
        Message::User { images, .. } if !images.is_empty()
    )));
    // History still holds the image — never permanently deleted.
    assert!(agent.history().iter().any(|entry| matches!(
        entry,
        SessionEntry::Message {
            message: Message::User { images, .. }
        } if !images.is_empty()
    )));
}

#[tokio::test]
async fn non_vision_compaction_strips_request_but_keeps_retained_images() {
    // Non-vision compaction: the summary request must be stripped (the wire
    // gate would otherwise reject it), but the persisted retained tail keeps
    // its images so a later vision model regains them.
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::new(
        Box::new(GateSimModel {
            replies: vec![AssistantMessage {
                content: Some("The user shared an image about an older topic; the assistant responded early in the conversation, and the work now continues with a fresh image request.".into()),
                tool_calls: vec![],
                reasoning: None,
            }],
            requests: requests.clone(),
            vision: false,
        }),
        vec![],
    );
    agent.restore_history(vec![
        Message::User {
            content: "old with image".into(),
            images: vec![ImagePart {
                hash: "beef".into(),
                mime: "image/png".into(),
            }],
        }
        .into(),
        Message::Assistant(AssistantMessage {
            content: Some("early answer".into()),
            tool_calls: vec![],
            reasoning: None,
        })
        .into(),
        Message::User {
            content: "current with image".into(),
            images: vec![ImagePart {
                hash: "cafe".into(),
                mime: "image/jpeg".into(),
            }],
        }
        .into(),
    ]);
    assert_eq!(
        agent.compact().await.unwrap(),
        "The user shared an image about an older topic; the assistant responded early in the conversation, and the work now continues with a fresh image request."
    );
    // The compaction request carried no images (the gate would have failed).
    let calls = requests.lock().unwrap();
    assert!(calls[0].iter().all(|message| !matches!(
        message,
        Message::User { images, .. } if !images.is_empty()
    )));
    // The retained tail kept its image reference.
    let compaction = agent
        .history()
        .iter()
        .find_map(|entry| match entry {
            SessionEntry::Compaction { retained, .. } => Some(retained),
            _ => None,
        })
        .expect("compaction entry");
    assert!(compaction.iter().any(|message| matches!(
        message,
        Message::User { content, images } if content == "current with image" && !images.is_empty()
    )));
}

#[tokio::test]
async fn vision_compaction_keeps_images_in_request_and_retained_tail() {
    // Vision compaction is lossless both ways: the summary request keeps the
    // image parts, and the retained tail keeps the latest User's image.
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::new(
        Box::new(VisionScriptedModel(ScriptedModel {
            replies: vec![AssistantMessage {
                content: Some("The user shared an image about an older topic; the assistant responded early in the conversation, and the work now continues with a fresh image request.".into()),
                tool_calls: vec![],
                reasoning: None,
            }],
            requests: requests.clone(),
            delays: Default::default(),
        })),
        vec![],
    );
    agent.restore_history(vec![
        Message::User {
            content: "old with image".into(),
            images: vec![ImagePart {
                hash: "beef".into(),
                mime: "image/png".into(),
            }],
        }
        .into(),
        Message::Assistant(AssistantMessage {
            content: Some("early answer".into()),
            tool_calls: vec![],
            reasoning: None,
        })
        .into(),
        Message::User {
            content: "current with image".into(),
            images: vec![ImagePart {
                hash: "cafe".into(),
                mime: "image/jpeg".into(),
            }],
        }
        .into(),
    ]);
    assert_eq!(
        agent.compact().await.unwrap(),
        "The user shared an image about an older topic; the assistant responded early in the conversation, and the work now continues with a fresh image request."
    );
    // The vision compaction request kept the image in the compacted prefix.
    let calls = requests.lock().unwrap();
    assert!(calls[0].iter().any(|message| matches!(
        message,
        Message::User { content, images } if content == "old with image" && !images.is_empty()
    )));
    // The retained tail kept the latest User's image reference.
    let compaction = agent
        .history()
        .iter()
        .find_map(|entry| match entry {
            SessionEntry::Compaction { retained, .. } => Some(retained),
            _ => None,
        })
        .expect("compaction entry");
    assert!(compaction.iter().any(|message| matches!(
        message,
        Message::User { content, images } if content == "current with image" && !images.is_empty()
    )));
}

#[tokio::test]
async fn compaction_rejects_summary_with_tool_call_markup() {
    // Degenerate model output that echoes the DSML rendering of tool calls
    // (with the `tool_calls` field empty, so the existing tool-calls check
    // cannot catch it) must fail the compaction sanity gate.
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::new(
        Box::new(ScriptedModel {
            replies: vec![AssistantMessage {
                content: Some(
                    "<invoke name=\"bash\">\n<parameter name=\"command\">make</parameter>\n</invoke>"
                        .into(),
                ),
                tool_calls: vec![],
                reasoning: None,
            }],
            requests: requests.clone(),
            delays: Default::default(),
        }),
        vec![],
    );
    agent.restore_history(vec![
        Message::User {
            content: "old question".into(),
            images: vec![],
        }
        .into(),
        Message::Assistant(AssistantMessage {
            content: Some("old answer".into()),
            tool_calls: vec![],
            reasoning: None,
        })
        .into(),
        Message::User {
            content: "current question".into(),
            images: vec![],
        }
        .into(),
    ]);
    let error = agent.compact().await.unwrap_err().to_string();
    assert!(
        error.contains("compaction summary rejected by sanity gate")
            && error.contains("tool-call/DSML markup"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn compaction_rejects_summary_echoing_retained_first_message() {
    // The model returns the retained user turn verbatim (the accident
    // shape: repeating the most recent context instead of summarizing).
    let echoed = "The user asked to build the feature and run the full test suite to verify everything passes before committing.";
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::new(
        Box::new(ScriptedModel {
            replies: vec![AssistantMessage {
                content: Some(echoed.into()),
                tool_calls: vec![],
                reasoning: None,
            }],
            requests: requests.clone(),
            delays: Default::default(),
        }),
        vec![],
    );
    agent.restore_history(vec![
        Message::User {
            content: "old question".into(),
            images: vec![],
        }
        .into(),
        Message::Assistant(AssistantMessage {
            content: Some("old answer".into()),
            tool_calls: vec![],
            reasoning: None,
        })
        .into(),
        Message::User {
            content: echoed.into(),
            images: vec![],
        }
        .into(),
    ]);
    let error = agent.compact().await.unwrap_err().to_string();
    assert!(
        error.contains("compaction summary rejected by sanity gate")
            && error.contains("echoes the retained context"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn compaction_rejects_too_short_summary() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::new(
        Box::new(ScriptedModel {
            replies: vec![AssistantMessage {
                content: Some("ok".into()),
                tool_calls: vec![],
                reasoning: None,
            }],
            requests: requests.clone(),
            delays: Default::default(),
        }),
        vec![],
    );
    agent.restore_history(vec![
        Message::User {
            content: "old question".into(),
            images: vec![],
        }
        .into(),
        Message::Assistant(AssistantMessage {
            content: Some("old answer".into()),
            tool_calls: vec![],
            reasoning: None,
        })
        .into(),
        Message::User {
            content: "current question".into(),
            images: vec![],
        }
        .into(),
    ]);
    let error = agent.compact().await.unwrap_err().to_string();
    assert!(
        error.contains("compaction summary rejected by sanity gate") && error.contains("too short"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn compaction_accepts_a_normal_summary() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let summary = "The user's original goal was to build the project with make; the assistant ran the build and its tests through bash, and no unfinished work remains before the latest user turn.";
    let mut agent = Agent::new(
        Box::new(ScriptedModel {
            replies: vec![AssistantMessage {
                content: Some(summary.into()),
                tool_calls: vec![],
                reasoning: None,
            }],
            requests: requests.clone(),
            delays: Default::default(),
        }),
        vec![],
    );
    agent.restore_history(vec![
        Message::User {
            content: "old question".into(),
            images: vec![],
        }
        .into(),
        Message::Assistant(AssistantMessage {
            content: Some("old answer".into()),
            tool_calls: vec![],
            reasoning: None,
        })
        .into(),
        Message::User {
            content: "current question".into(),
            images: vec![],
        }
        .into(),
    ]);
    let output = agent.prepare_compaction().await.unwrap();
    assert_eq!(output.summary, summary);
    assert!(matches!(
        output.entry,
        SessionEntry::Compaction { summary: s, .. } if s == summary
    ));
    // The summary prompt must carry the plain-text guard so the model does
    // not leak DSML/tool-call markup into the persisted summary.
    let requests = requests.lock().unwrap();
    assert!(matches!(
        requests[0].last().unwrap(),
        Message::User { content, .. }
            if content.contains("Output a plain-text summary only")
                && content.contains("no tool calls, no XML/DSML/function-call markup, no code blocks")
    ));
}

#[tokio::test]
async fn compaction_rejects_summary_echoing_request_tail_assistant() {
    // The accident shape: the model repeats the last compacted message
    // verbatim — here the assistant message at the end of the request
    // window (the message right before the retained current turn). The
    // retained-first echo check cannot catch this (the summary shares no
    // prefix with the retained user turn), so the request-tail comparison
    // must reject it.
    let echoed = "The user asked for a detailed plan to refactor the legacy module, and the assistant outlined the steps and ran the initial checks before reporting back.";
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::new(
        Box::new(ScriptedModel {
            replies: vec![AssistantMessage {
                content: Some(echoed.into()),
                tool_calls: vec![],
                reasoning: None,
            }],
            requests: requests.clone(),
            delays: Default::default(),
        }),
        vec![],
    );
    agent.restore_history(vec![
        Message::User {
            content: "old question".into(),
            images: vec![],
        }
        .into(),
        Message::Assistant(AssistantMessage {
            content: Some(echoed.into()),
            tool_calls: vec![],
            reasoning: None,
        })
        .into(),
        Message::User {
            content: "current question".into(),
            images: vec![],
        }
        .into(),
    ]);
    let error = agent.compact().await.unwrap_err().to_string();
    assert!(
        error.contains("compaction summary rejected by sanity gate") && error.contains("echoes"),
        "unexpected error: {error}"
    );
}
#[tokio::test]
async fn switching_back_to_vision_restores_images_in_requests() {
    // A non-vision round strips the image from the request only; history
    // keeps it, and after switching back to a vision model the image reaches
    // the next request again.
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::new(
        Box::new(ScriptedModel {
            replies: vec![AssistantMessage {
                content: Some("first".into()),
                tool_calls: vec![],
                reasoning: None,
            }],
            requests: requests.clone(),
            delays: Default::default(),
        }),
        vec![],
    );
    agent.restore_history(vec![
        Message::User {
            content: "look at this".into(),
            images: vec![ImagePart {
                hash: "cafe".into(),
                mime: "image/webp".into(),
            }],
        }
        .into(),
    ]);
    agent.complete_round(&[]).await.unwrap();
    {
        let calls = requests.lock().unwrap();
        assert!(calls[0].iter().all(|message| !matches!(
            message,
            Message::User { images, .. } if !images.is_empty()
        )));
    }
    assert!(agent.history().iter().any(|entry| matches!(
        entry,
        SessionEntry::Message {
            message: Message::User { images, .. }
        } if !images.is_empty()
    )));
    // Switch back to a vision model: the image is restored on the wire.
    agent.set_model(Box::new(VisionScriptedModel(ScriptedModel {
        replies: vec![AssistantMessage {
            content: Some("second".into()),
            tool_calls: vec![],
            reasoning: None,
        }],
        requests: requests.clone(),
        delays: Default::default(),
    })));
    agent.complete_round(&[]).await.unwrap();
    let calls = requests.lock().unwrap();
    assert!(calls[1].iter().any(|message| matches!(
        message,
        Message::User { images, .. } if !images.is_empty()
    )));
}

#[test]
fn context_includes_user_images() {
    let mut agent = Agent::new(
        Box::new(ScriptedModel {
            replies: vec![],
            requests: Arc::new(Mutex::new(Vec::new())),
            delays: Default::default(),
        }),
        vec![],
    );
    agent.restore_history(vec![
        Message::User {
            content: "look at this".into(),
            images: vec![ImagePart {
                hash: "cafe".into(),
                mime: "image/webp".into(),
            }],
        }
        .into(),
    ]);
    let context = agent.context();
    assert_eq!(context.len(), 1);
    match &context[0] {
        Message::User { content, images } => {
            assert_eq!(content, "look at this");
            assert_eq!(images.len(), 1);
            assert_eq!(images[0].hash, "cafe");
            assert_eq!(images[0].mime, "image/webp");
        }
        other => panic!("expected user message, got {other:?}"),
    }
}

#[test]
fn image_store_dedups_by_hash_and_mime_whitelist() {
    let temp = tempfile::tempdir().unwrap();
    let bytes = b"same-content";
    let first = store_image_bytes(temp.path(), bytes).unwrap();
    let second = store_image_bytes(temp.path(), bytes).unwrap();
    assert_eq!(first, second);
    assert_eq!(std::fs::read(temp.path().join(&first)).unwrap(), bytes);
    // Directory contains exactly one file.
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);

    assert_eq!(image_mime_from_extension("a.PNG"), Some("image/png"));
    assert_eq!(image_mime_from_extension("a.jpg"), Some("image/jpeg"));
    assert_eq!(image_mime_from_extension("a.jpeg"), Some("image/jpeg"));
    assert_eq!(image_mime_from_extension("a.webp"), Some("image/webp"));
    assert_eq!(image_mime_from_extension("a.gif"), Some("image/gif"));
    assert_eq!(image_mime_from_extension("a.txt"), None);
    assert_eq!(image_mime_from_extension("a.png.txt"), None);
    assert_eq!(image_mime_from_extension("noextension"), None);

    assert_eq!(
        load_image_bytes(Some(temp.path()), &first),
        Some(bytes.to_vec())
    );
    assert_eq!(load_image_bytes(Some(temp.path()), "missing"), None);
    assert_eq!(load_image_bytes(None, &first), None);
}

// ── strip_images (compaction image stripping) ────────────────────────

#[test]
fn strip_images_clears_user_images_keeps_content_and_role() {
    let mut messages = vec![
        Message::System {
            content: "sys".into(),
        },
        Message::User {
            content: "with image".into(),
            images: vec![ImagePart {
                hash: "deadbeef".into(),
                mime: "image/png".into(),
            }],
        },
        Message::Assistant(AssistantMessage {
            content: Some("ok".into()),
            tool_calls: vec![],
            reasoning: None,
        }),
        Message::Tool {
            call_id: "1".into(),
            name: "read_image".into(),
            content: "[image read: a.png]".into(),
            is_error: false,
            synthetic: false,
        },
        Message::User {
            content: "plain".into(),
            images: vec![],
        },
    ];
    strip_images(&mut messages);
    assert_eq!(
        messages,
        vec![
            Message::System {
                content: "sys".into(),
            },
            Message::User {
                content: "with image".into(),
                images: vec![],
            },
            Message::Assistant(AssistantMessage {
                content: Some("ok".into()),
                tool_calls: vec![],
                reasoning: None,
            }),
            Message::Tool {
                call_id: "1".into(),
                name: "read_image".into(),
                content: "[image read: a.png]".into(),
                is_error: false,
                synthetic: false,
            },
            Message::User {
                content: "plain".into(),
                images: vec![],
            },
        ]
    );
}
