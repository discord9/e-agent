//! The always-on, current-session-only history tool.
//!
//! The runner supplies the store, workspace root, and session id. The tool
//! deliberately uses the existing logical `SessionStore::load_with_seq`
//! view: duplicate physical rows are not exposed separately.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::{Message, SessionEntry, Tool, ToolOutput, ToolSpec};
use crate::session_store::SessionStore;

/// Default and maximum number of logical entries returned by `list` and
/// `search`.
pub const HISTORY_DEFAULT_LIMIT: usize = 20;
pub const HISTORY_MAX_LIMIT: usize = 100;

/// Marker tool. Calls are intercepted by the session runner because only it
/// owns the current session's store binding.
pub struct History;

#[async_trait]
impl Tool for History {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "history".into(),
            description: "Read the current session's logical transcript. Actions are `list` (newest bounded entries), `read` (one complete persisted entry by seq, including outside the list window), and `search` (case-sensitive literal search of user, assistant, and notice text in the newest bounded slice). Duplicate physical writes and same-seq replacements use the existing logical winner semantics.".into(),
            parameters: json!({
                "oneOf": [
                    {
                        "type": "object",
                        "properties": {
                            "action": {"const": "list"},
                            "limit": {"type": "integer", "minimum": 1, "maximum": HISTORY_MAX_LIMIT}
                        },
                        "required": ["action"],
                        "additionalProperties": false
                    },
                    {
                        "type": "object",
                        "properties": {
                            "action": {"const": "read"},
                            "seq": {"type": "integer"}
                        },
                        "required": ["action", "seq"],
                        "additionalProperties": false
                    },
                    {
                        "type": "object",
                        "properties": {
                            "action": {"const": "search"},
                            "query": {"type": "string", "minLength": 1},
                            "limit": {"type": "integer", "minimum": 1, "maximum": HISTORY_MAX_LIMIT}
                        },
                        "required": ["action", "query"],
                        "additionalProperties": false
                    }
                ]
            }),
        }
    }

    async fn execute(&self, _: Value) -> Result<ToolOutput, String> {
        Err("history is executed by the session runner".into())
    }
}

/// Parse and execute a history call against the runner's current binding.
pub async fn execute(
    store: &SessionStore,
    root: &std::path::Path,
    session: &str,
    arguments: &Value,
) -> Result<String, String> {
    let (action, limit, seq, query) = parse_arguments(arguments)?;
    let entries = store.load_with_seq(root, session).await.map_err(|error| {
        // Store errors can contain paths, connection strings, and anyhow
        // chains. Keep those in tracing only, as read_output does.
        tracing::error!(session = %session, "history: cannot load session entries: {error:#}");
        "cannot load session history".to_owned()
    })?;

    match action.as_str() {
        "list" => {
            let entries: Vec<Value> = ordered_entries(&entries)
                .into_iter()
                .take(limit)
                .map(|(seq, entry)| json!({"seq": seq, "entry": entry}))
                .collect();
            Ok(json!({"entries": entries}).to_string())
        }
        "read" => {
            let seq = seq.expect("read requires seq after parsing");
            let Some((_, entry)) = entries.iter().find(|(entry_seq, _)| *entry_seq == seq) else {
                return Err("history entry not found".into());
            };
            Ok(json!({"seq": seq, "entry": entry}).to_string())
        }
        "search" => {
            let query = query.expect("search requires query after parsing");
            let entries: Vec<Value> = ordered_entries(&entries)
                .into_iter()
                .take(HISTORY_MAX_LIMIT)
                .filter(|(_, entry)| {
                    searchable_content(entry).is_some_and(|content| content.contains(&query))
                })
                .take(limit)
                .map(|(seq, entry)| json!({"seq": seq, "entry": entry}))
                .collect();
            Ok(json!({"entries": entries}).to_string())
        }
        _ => unreachable!("parse_arguments validates action"),
    }
}

/// Sort the already-logical store result newest-first by seq. Backends load
/// by their established transcript order, which is not the History API order.
fn ordered_entries(entries: &[(i64, SessionEntry)]) -> Vec<(i64, &SessionEntry)> {
    let mut ordered: Vec<_> = entries.iter().map(|(seq, entry)| (*seq, entry)).collect();
    ordered.sort_by(|left, right| right.0.cmp(&left.0));
    ordered
}

fn searchable_content(entry: &SessionEntry) -> Option<&str> {
    match entry {
        SessionEntry::Message { message } => match message {
            Message::User { content, .. } => Some(content),
            Message::Assistant(assistant) => assistant.content.as_deref(),
            _ => None,
        },
        SessionEntry::Notice { text } => Some(text),
        _ => None,
    }
}

fn parse_arguments(
    arguments: &Value,
) -> Result<(String, usize, Option<i64>, Option<String>), String> {
    let object = arguments
        .as_object()
        .ok_or("history arguments must be a JSON object")?;
    let action = object
        .get("action")
        .and_then(Value::as_str)
        .ok_or("history requires `action` (`list`, `read`, or `search`)")?
        .to_owned();
    let allowed: &[&str] = match action.as_str() {
        "list" => &["action", "limit"],
        "read" => &["action", "seq"],
        "search" => &["action", "query", "limit"],
        _ => {
            return Err(format!(
                "unknown history action `{action}` (known: list, read, search)"
            ));
        }
    };
    let unknown: Vec<&str> = object
        .keys()
        .filter(|key| !allowed.contains(&key.as_str()))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "history received unknown field(s): {}",
            unknown
                .iter()
                .map(|key| format!("`{key}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let limit = match object.get("limit") {
        None => HISTORY_DEFAULT_LIMIT,
        Some(value) => {
            let limit = value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or("history `limit` must be a non-negative integer")?;
            if !(1..=HISTORY_MAX_LIMIT).contains(&limit) {
                return Err(format!(
                    "history `limit` must be between 1 and {HISTORY_MAX_LIMIT}"
                ));
            }
            limit
        }
    };
    let seq = match object.get("seq") {
        None => None,
        Some(value) => Some(value.as_i64().ok_or("history `seq` must be an integer")?),
    };
    let query = match object.get("query") {
        None => None,
        Some(value) => {
            let query = value.as_str().ok_or("history `query` must be a string")?;
            if query.is_empty() {
                return Err("history `query` must not be empty".into());
            }
            Some(query.to_owned())
        }
    };
    if action == "read" && seq.is_none() {
        return Err("history read requires `seq` (an integer)".into());
    }
    if action == "search" && query.is_none() {
        return Err("history search requires `query` (a non-empty string)".into());
    }
    Ok((action, limit, seq, query))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AssistantMessage, SessionEntry};

    fn notice(text: &str) -> SessionEntry {
        SessionEntry::Notice { text: text.into() }
    }

    fn user(content: &str) -> SessionEntry {
        SessionEntry::Message {
            message: Message::User {
                content: content.into(),
                images: vec![],
            },
        }
    }

    fn assistant(content: &str) -> SessionEntry {
        SessionEntry::Message {
            message: Message::Assistant(AssistantMessage {
                content: Some(content.into()),
                tool_calls: vec![],
                reasoning: None,
            }),
        }
    }

    #[test]
    fn schema_is_current_session_only_and_closed() {
        let parameters = History.spec().parameters;
        assert_eq!(parameters["oneOf"].as_array().unwrap().len(), 3);
        assert_eq!(
            parameters["oneOf"][0]["properties"]["action"]["const"],
            json!("list")
        );
        assert_eq!(
            parameters["oneOf"][1]["properties"]["action"]["const"],
            json!("read")
        );
        assert_eq!(
            parameters["oneOf"][2]["properties"]["action"]["const"],
            json!("search")
        );
        for variant in parameters["oneOf"].as_array().unwrap() {
            assert_eq!(variant["additionalProperties"], json!(false));
            assert!(variant["properties"].get("workspace").is_none());
            assert!(variant["properties"].get("session_id").is_none());
        }
        assert_eq!(
            parameters["oneOf"][0]["properties"]["limit"]["maximum"],
            json!(100)
        );
        assert_eq!(
            parameters["oneOf"][2]["properties"]["limit"]["maximum"],
            json!(100)
        );
    }

    #[test]
    fn action_specific_validation_rejects_invalid_fields() {
        assert!(parse_arguments(&json!({"action": "list", "seq": 1})).is_err());
        assert!(parse_arguments(&json!({"action": "read", "seq": 1, "limit": 2})).is_err());
        assert!(parse_arguments(&json!({"action": "search", "query": "x", "seq": 1})).is_err());
        assert!(parse_arguments(&json!({"action": "search", "query": ""})).is_err());
        assert!(parse_arguments(&json!({"action": "search"})).is_err());
        assert!(parse_arguments(&json!({"action": "search", "query": "x", "limit": 0})).is_err());
        assert!(parse_arguments(&json!({"action": "read"})).is_err());
    }

    #[tokio::test]
    async fn list_is_newest_first_and_capped_without_pagination_fields() {
        let temp = tempfile::tempdir().unwrap();
        let entries: Vec<_> = (0..25).map(|index| notice(&index.to_string())).collect();
        SessionStore::Jsonl
            .append(temp.path(), "current", &entries)
            .await
            .unwrap();
        let result: Value = serde_json::from_str(
            &execute(
                &SessionStore::Jsonl,
                temp.path(),
                "current",
                &json!({"action": "list", "limit": 3}),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            result["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["seq"].as_i64().unwrap())
                .collect::<Vec<_>>(),
            vec![24, 23, 22]
        );
        assert!(result.get("next_cursor").is_none());
        assert!(result.get("has_more").is_none());
    }

    #[tokio::test]
    async fn search_is_literal_whitelisted_bounded_ordered_and_limited() {
        let temp = tempfile::tempdir().unwrap();
        let mut entries = vec![user("old needle")];
        entries.extend((1..100).map(|index| {
            if index == 99 {
                notice("older needle")
            } else if index == 98 {
                notice("another needle")
            } else {
                notice(&format!("filler {index}"))
            }
        }));
        entries.push(user("new NEEDLE literal"));
        entries.push(assistant("needle assistant"));
        let excluded_start = entries.len() as i64;
        entries.push(SessionEntry::Message {
            message: Message::Tool {
                call_id: "tool".into(),
                name: "tool".into(),
                content: "needle excluded".into(),
                images: vec![],
                is_error: false,
                synthetic: false,
            },
        });
        entries.push(SessionEntry::Compaction {
            summary: "needle excluded".into(),
            retained: vec![],
            current_prompt_at: None,
            no_current_prompt: false,
        });
        entries.push(SessionEntry::BackgroundCompletion {
            id: 1,
            output: "needle excluded".into(),
            label: None,
            started_at_ms: None,
            duration_ms: None,
            exit_code: None,
            signal: None,
            status: None,
            kind: None,
        });
        entries.push(SessionEntry::Error {
            text: "needle excluded".into(),
        });
        SessionStore::Jsonl
            .append(temp.path(), "current", &entries)
            .await
            .unwrap();

        let result: Value = serde_json::from_str(
            &execute(
                &SessionStore::Jsonl,
                temp.path(),
                "current",
                &json!({"action": "search", "query": "needle", "limit": 2}),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        let found = result["entries"].as_array().unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0]["seq"], json!(101)); // assistant, newest matching
        assert_eq!(found[1]["seq"], json!(99)); // newest-100 scan bound excludes seq 0
        assert_eq!(found[0]["entry"]["type"], json!("message"));
        assert_eq!(found[1]["entry"]["type"], json!("notice"));

        let all_matches: Value = serde_json::from_str(
            &execute(
                &SessionStore::Jsonl,
                temp.path(),
                "current",
                &json!({"action": "search", "query": "needle", "limit": 100}),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        let all_matches = all_matches["entries"].as_array().unwrap();
        let excluded_seqs = (excluded_start..excluded_start + 4).collect::<Vec<_>>();
        assert!(
            all_matches
                .iter()
                .all(|entry| { !excluded_seqs.contains(&entry["seq"].as_i64().unwrap()) })
        );
        assert_eq!(
            all_matches
                .iter()
                .map(|entry| (
                    entry["seq"].as_i64().unwrap(),
                    entry["entry"]["type"].as_str().unwrap(),
                ))
                .collect::<Vec<_>>(),
            vec![(101, "message"), (99, "notice"), (98, "notice")]
        );

        let empty: Value = serde_json::from_str(
            &execute(
                &SessionStore::Jsonl,
                temp.path(),
                "current",
                &json!({"action": "search", "query": "NEEDLE"}),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!(empty["entries"].as_array().unwrap().len(), 1);
        assert_eq!(empty["entries"][0]["seq"], json!(100));
    }

    #[tokio::test]
    async fn read_returns_complete_entry_and_not_found_and_empty_list() {
        let temp = tempfile::tempdir().unwrap();
        let complete = notice(&"x".repeat(5_000));
        SessionStore::Jsonl
            .append(temp.path(), "current", std::slice::from_ref(&complete))
            .await
            .unwrap();
        let read: Value = serde_json::from_str(
            &execute(
                &SessionStore::Jsonl,
                temp.path(),
                "current",
                &json!({"action": "read", "seq": 0}),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!(read["entry"], serde_json::to_value(complete).unwrap());
        assert_eq!(
            execute(
                &SessionStore::Jsonl,
                temp.path(),
                "current",
                &json!({"action": "read", "seq": 9})
            )
            .await
            .unwrap_err(),
            "history entry not found"
        );
        let empty: Value = serde_json::from_str(
            &execute(
                &SessionStore::Jsonl,
                temp.path(),
                "empty",
                &json!({"action": "list"}),
            )
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!(empty, json!({"entries": []}));
    }

    #[tokio::test]
    async fn selector_fields_are_rejected_before_store_access_for_each_session() {
        let temp = tempfile::tempdir().unwrap();
        for session in ["one", "two"] {
            let error = execute(
                &SessionStore::Jsonl,
                temp.path(),
                session,
                &json!({
                    "action": "list", "workspace": "other", "session": "other",
                }),
            )
            .await
            .unwrap_err();
            assert!(error.contains("unknown field"));
        }
    }
}
