use serde_json::{Value, json};

use crate::agent::{Tool, ToolOutput, ToolSpec};

/// Marker tool: the session runner pauses for a human answer.
pub(crate) struct RequestUserInput;

#[async_trait::async_trait]
impl Tool for RequestUserInput {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "request_user_input".into(),
            description: "Pause and ask the user one question.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {"type": "string"},
                                "prompt": {"type": "string"}
                            },
                            "required": ["id", "prompt"],
                            "additionalProperties": false
                        },
                        "minItems": 1,
                        "maxItems": 1
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

pub(crate) fn parse_questions(arguments: &str) -> Result<Vec<crate::runner::UserQuestion>, String> {
    let value: Value = serde_json::from_str(arguments)
        .map_err(|_| "request_user_input arguments must be valid JSON".to_owned())?;
    let object = value
        .as_object()
        .ok_or_else(|| "request_user_input arguments must be an object".to_owned())?;
    if object.keys().any(|key| key != "questions") {
        return Err("request_user_input accepts only `questions`".into());
    }
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
    if question.keys().any(|key| key != "id" && key != "prompt") {
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
    Ok(vec![crate::runner::UserQuestion {
        id: id.into(),
        prompt: prompt.into(),
    }])
}

fn is_safe_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
}
