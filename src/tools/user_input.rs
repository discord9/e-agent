use serde_json::{Value, json};

use crate::agent::{Tool, ToolOutput, ToolSpec};

/// Marker tool: the session runner pauses for a human answer (blocking) or
/// registers a process-local question and returns immediately (`async`).
pub(crate) struct RequestUserInput;

#[async_trait::async_trait]
impl Tool for RequestUserInput {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "request_user_input".into(),
            description: "Ask the user one question. Blocking mode (the default) pauses this turn \
                          until the user answers. With `async: true` the call returns immediately \
                          with a receipt so you can continue; the user's answer arrives later as a \
                          user message that quotes the original question. A question may carry \
                          `options`: when present the user picks one and the answer is that \
                          option's `id`; when absent the user types free text."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {"type": "string"},
                                "prompt": {"type": "string"},
                                "options": {
                                    "type": "array",
                                    "description": "Fixed choices. When present the user picks one and the answer is that option's `id`; when absent the user types free text.",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "id": {"type": "string"},
                                            "label": {"type": "string"},
                                            "description": {"type": "string"}
                                        },
                                        "required": ["id", "label"],
                                        "additionalProperties": false
                                    },
                                    "minItems": 1
                                }
                            },
                            "required": ["id", "prompt"],
                            "additionalProperties": false
                        },
                        "minItems": 1,
                        "maxItems": 1
                    },
                    "async": {
                        "type": "boolean",
                        "description": "Do not pause this turn; the answer arrives later as a user message."
                    }
                },
                "required": ["questions"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, _: Value) -> Result<ToolOutput, String> {
        Err("request_user_input is executed by the session runner".into())
    }
}

/// One parsed `request_user_input` call: exactly one question plus the
/// blocking/async mode. `async: true` never parks the turn; the runner
/// registers the question in process memory and answers arrive later as a
/// durable user-shaped Notice.
pub(crate) struct ParsedQuestion {
    pub(crate) question: crate::runner::UserQuestion,
    pub(crate) is_async: bool,
}

pub(crate) fn parse_questions(arguments: &str) -> Result<ParsedQuestion, String> {
    let value: Value = serde_json::from_str(arguments)
        .map_err(|_| "request_user_input arguments must be valid JSON".to_owned())?;
    let object = value
        .as_object()
        .ok_or_else(|| "request_user_input arguments must be an object".to_owned())?;
    if object
        .keys()
        .any(|key| key != "questions" && key != "async")
    {
        return Err("request_user_input accepts only `questions` and `async`".into());
    }
    let is_async = match object.get("async") {
        None => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err("request_user_input `async` must be a boolean".into()),
    };
    let questions = object
        .get("questions")
        .and_then(Value::as_array)
        .ok_or_else(|| "request_user_input requires `questions`".to_owned())?;
    if questions.len() != 1 {
        return Err("request_user_input requires exactly one question".into());
    }
    let question = questions[0]
        .as_object()
        .ok_or_else(|| "request_user_input question must be an object".to_owned())?;
    if question
        .keys()
        .any(|key| key != "id" && key != "prompt" && key != "options")
    {
        return Err("request_user_input question has unsupported fields".into());
    }
    let id = question
        .get("id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty() && value.chars().all(is_safe_id_char))
        .ok_or_else(|| "request_user_input question id must be a non-empty safe id".to_owned())?;
    let prompt = question
        .get("prompt")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "request_user_input question prompt must be non-empty".to_owned())?;
    let options = parse_options(question.get("options"))?;
    Ok(ParsedQuestion {
        question: crate::runner::UserQuestion {
            id: id.into(),
            prompt: prompt.into(),
            options,
        },
        is_async,
    })
}

/// Parse the optional fixed `options` of one question. Absent means today's
/// free-text question; a present list must be non-empty with unique,
/// non-empty ids and non-empty labels.
fn parse_options(value: Option<&Value>) -> Result<Vec<crate::runner::UserOption>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| "request_user_input `options` must be an array".to_owned())?;
    if items.is_empty() {
        return Err("request_user_input `options` must contain at least one option".into());
    }
    let mut options: Vec<crate::runner::UserOption> = Vec::with_capacity(items.len());
    for item in items {
        let object = item
            .as_object()
            .ok_or_else(|| "request_user_input option must be an object".to_owned())?;
        if object
            .keys()
            .any(|key| key != "id" && key != "label" && key != "description")
        {
            return Err("request_user_input option has unsupported fields".into());
        }
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "request_user_input option id must be non-empty".to_owned())?;
        let label = object
            .get("label")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "request_user_input option label must be non-empty".to_owned())?;
        let description = match object.get("description") {
            None => None,
            Some(Value::String(text)) => Some(text.clone()),
            Some(_) => {
                return Err("request_user_input option description must be a string".into());
            }
        };
        if options.iter().any(|option| option.id == id) {
            return Err(format!(
                "request_user_input option ids must be unique: {id}"
            ));
        }
        options.push(crate::runner::UserOption {
            id: id.into(),
            label: label.into(),
            description,
        });
    }
    Ok(options)
}

fn is_safe_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
}
